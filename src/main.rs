#![feature(
    async_await,
    await_macro,
    futures_api,
    generators,
    gen_future,
    integer_atomics
)]

use {
    bytes::Bytes,
    futures::{channel::mpsc, compat::Future01CompatExt},
    hyper::{
        self,
        client::{connect::dns::TokioThreadpoolGaiResolver, HttpConnector},
        header::CONTENT_TYPE,
        Body, Client, Request, Response,
    },
    hyper_tls::HttpsConnector,
    kuchiki::traits::TendrilSink,
    mime::Mime,
    native_tls::TlsConnector,
    probabilistic_collections::cuckoo::CuckooFilter,
    slab::Slab,
    std::{
        collections::{HashSet, VecDeque},
        path::Path,
        pin::Pin,
        str,
        sync::{
            atomic::{AtomicU32, Ordering},
            Arc,
        },
        task::{LocalWaker, Poll, Waker},
    },
    tokio::prelude::Stream,
    url::Url,
};

type SClient = Client<HttpsConnector<HttpConnector<TokioThreadpoolGaiResolver>>>;

pub struct UrlPool {
    pool: VecDeque<Url>,
}

impl UrlPool {
    pub fn add_urls(&mut self, urls: Vec<Url>) {
        self.pool.extend(urls);
    }
    fn try_get_url(&mut self) -> Result<Url, GetUrlError> {
        match self.pool.pop_front() {
            Some(url) => Ok(url),
            None => Err(GetUrlError::IsEmpty),
        }
    }
}

enum GetUrlError {
    IsEmpty,
}

pub struct UrlStream {
    source: UrlPool,
    inflight: AtomicU32, //TODO: Share this with other data structures? Probably should, because the seed URLs are put into the UrlPool...or should they be?
    waker: Option<Waker>,
}

impl futures::stream::Stream for UrlStream {
    type Item = Url;

    fn poll_next(mut self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        match this.source.try_get_url() {
            Ok(url) => {
                this.inflight.fetch_add(1, Ordering::SeqCst);
                Poll::Ready(Some(url))
            }
            Err(_) => {
                if this.inflight.load(Ordering::SeqCst) == 0 {
                    Poll::Ready(None)
                } else {
                    this.waker = Some(lw.clone().into_waker());
                    Poll::Pending
                }
            }
        }
    }
}

pub struct CrawlResourcePool {
    data: Slab<CrawlData>,
    url_receiver: mpsc::UnboundedReceiver<CrawlData>,
}

pub struct CrawlData {
    source: Arc<Url>,
    data_type: Option<Mime>,
    data: Bytes,
}

pub struct UrlStatus {
    url: Arc<Url>,
    error: hyper::error::Error,
}

async fn process_url(
    url: Arc<Url>,
    client: Arc<SClient>,
    crawl_data_sender: mpsc::UnboundedSender<CrawlData>,
    url_status_sender: mpsc::UnboundedSender<UrlStatus>,
) {
    let req = Request::builder()
            .method("GET")
            .uri(&url[..])
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/71.0.3578.98 Safari/537.36")
            .body(Body::empty())
            .unwrap();
    let resp = {
        match await!(client.clone().request(req).compat()) {
            Ok(x) => x,
            Err(error) => {
                url_status_sender.unbounded_send(UrlStatus {
                    url: url.clone(),
                    error,
                });
                return;
            }
        }
    };
    let headers = resp.headers();
    let data_type: Option<Mime> = if let Some(ct) = headers.get(CONTENT_TYPE).cloned() {
        let mime_str = ct.to_str();
        if mime_str.is_err() {
            None
        } else {
            let mime = mime_str.unwrap().parse::<Mime>();
            if mime.is_err() {
                let ending = url.path_segments().unwrap();
                let ending = ending.last().unwrap();
                mime_guess::guess_mime_type_opt(Path::new(ending))
            } else {
                Some(mime.unwrap())
            }
        }
    } else {
        None
    };
    let data: Bytes = {
        match await!(resp.into_body().concat2().compat()) {
            Ok(x) => x.into(),
            Err(error) => {
                url_status_sender.unbounded_send(UrlStatus {
                    url: url.clone(),
                    error,
                });
                return;
            }
        }
    };
    let crawl_data = CrawlData {
        source: url.clone(),
        data_type,
        data,
    };
    crawl_data_sender.unbounded_send(crawl_data);
}

pub struct UrlSearcher {
    pub crawl_url: Url,
    pub base_url: Url,
    pub data: Response<Body>,
}

impl UrlSearcher {
    pub async fn crawl_html(self) -> HashSet<Url> {
        let body = await!(self.data.into_body().concat2().compat()).unwrap();
        let body = str::from_utf8(&body[..]).unwrap();
        let parser = kuchiki::parse_html().one(&body[..]);
        let link_selector = parser.select("a").unwrap();
        let mut urls = HashSet::new();
        for found_link in link_selector {
            let link_attribs = &(&*found_link).attributes;
            let link_attribs_borrow = link_attribs.borrow();
            let url = if let Some(url) = link_attribs_borrow.get("href") {
                match Url::parse(url) {
                    Err(error) if error == url::ParseError::RelativeUrlWithoutBase => {
                        self.base_url.join(url).unwrap()
                    }
                    Err(error) => {
                        println!("Ignoring uri error {:?}. Discarding uri {:?}.", error, url);
                        continue;
                    }
                    Ok(url) => {
                        let host = url.host_str();
                        if let Some(host) = host {
                            if host != &self.base_url[..] {
                                println!("Skipping {:?} because we're only interested in what's on the EAG website.", url);
                                continue;
                            } else {
                                url
                            }
                        } else {
                            println!("Skipping {:?} url because it has no host. Is that even supposed to be possible?", url);
                            continue;
                        }
                    }
                }
            } else {
                continue;
            };
            urls.insert(url);
        }
        urls
    }
}

fn main() {}
