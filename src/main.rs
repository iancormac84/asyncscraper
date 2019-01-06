#![feature(async_await, await_macro, futures_api, integer_atomics)]

use {
    futures::{
        channel::mpsc,
        compat::Future01CompatExt,
        future::{FutureExt, TryFutureExt},
    },
    hyper::{
        client::{connect::dns::TokioThreadpoolGaiResolver, HttpConnector},
        header::CONTENT_TYPE,
        Body, Client, Request, Response,
    },
    hyper_tls::HttpsConnector,
    kuchiki::traits::TendrilSink,
    mime::Mime,
    native_tls::TlsConnector,
    parking_lot::Mutex,
    std::{
        collections::{HashSet, VecDeque},
        future::Future,
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
    pool: Mutex<VecDeque<Url>>,
}

impl UrlPool {
    pub fn add_urls(&self, urls: Vec<Url>) {
        let mut acquired = self.pool.lock();
        acquired.extend(urls);
    }
    fn try_get_url(&self) -> Result<Url, GetUrlError> {
        let popped_url = match self.pool.try_lock() {
            Some(mut pool) => pool.pop_front(),
            None => return Err(GetUrlError::IsLocked),
        };

        match popped_url {
            Some(url) => Ok(url),
            None => Err(GetUrlError::IsEmpty),
        }
    }
}

enum GetUrlError {
    IsLocked,
    IsEmpty,
    //IllFormattedUrl,
    //IsClosed,
}

pub struct UrlStream {
    source: Arc<UrlPool>,
    inflight: Arc<AtomicU32>,
    waker: Option<Waker>,
}

impl futures::stream::Stream for UrlStream {
    type Item = Url;

    fn poll_next(self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Option<Self::Item>> {
        match self.source.try_get_url() {
            Ok(url) => Poll::Ready(Some(url)),
            Err(e) => match e {
                GetUrlError::IsEmpty => {
                    if self.inflight.load(Ordering::SeqCst) == 0 {
                        Poll::Ready(None)
                    } else {
                        self.get_mut().waker = Some(lw.clone().into_waker());
                        Poll::Pending
                    }
                }
                GetUrlError::IsLocked => {
                    self.get_mut().waker = Some(lw.clone().into_waker());
                    Poll::Pending
                }
            },
        }
    }
}

pub struct Orchestrator {
    client: Arc<SClient>,
    to_visit: VecDeque<Url>,
    visited: VecDeque<Url>,
    inflight: Arc<AtomicU32>,
    max_inflight: u32,
    receiver: mpsc::UnboundedReceiver<HashSet<Url>>,
    sender: mpsc::UnboundedSender<HashSet<Url>>,
}

impl Orchestrator {
    pub fn new(urls: Vec<String>, max_inflight: u32) -> Orchestrator {
        let (sender, receiver) = mpsc::unbounded();
        Orchestrator {
            client: {
                let tls_connector = TlsConnector::new().unwrap();
                let connector = HttpConnector::new_with_tokio_threadpool_resolver();
                let mut connector = HttpsConnector::from((connector, tls_connector));
                connector.https_only(false);
                Arc::new(Client::builder().keep_alive(true).build(connector))
            },
            to_visit: urls.into_iter().map(|x| x.parse().unwrap()).collect(),
            visited: VecDeque::new(),
            inflight: Arc::new(AtomicU32::new(0)),
            max_inflight,
            sender,
            receiver,
        }
    }
}

impl Future for Orchestrator {
    type Output = ();
    fn poll(self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Self::Output> {
        if self.to_visit.is_empty() && self.inflight.load(Ordering::SeqCst) == 0 {
            return Poll::Ready(());
        }
        while !self.to_visit.is_empty() && self.inflight.load(Ordering::SeqCst) < self.max_inflight
        {
            return Poll::Pending;
        }
        Poll::Pending
    }
}
/*    pub async fn crawl_urls(&self) {
    for url in self.to_visit {
        let request = Request::builder()
            .method("GET")
            .uri(&url[..])
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/71.0.3578.98 Safari/537.36")
            .body(Body::empty())
            .unwrap();
        await!(self.process_request(request, url.clone()));
        println!("Back at the top!");
    }
}

async fn process_request(&self, req: Request<Body>, url: Url) {
    let resp = await!(self.client.clone().request(req).compat()).unwrap();

    let headers = resp.headers();
    //TODO: Do I need to determine the length? Maybe I can implement some
    //optimizations having to do with polling priority? In other words, say I
    //notice that a response has very little data. Maybe I could poll it more
    //eagerly to get it out of the way so that I can free up more clients in the
    //event that I have a limiter on the number of in-flight request/response pairs.
    /*let data_length: Option<u64> = if let Some(cl) = headers.get(CONTENT_LENGTH).cloned() {
        let num_str = cl.to_str();
        if num_str.is_err() {
            None
        } else {
            let num = num_str.unwrap().parse::<u64>();
            if num.is_err() {
                None
            } else {
                Some(num.unwrap())
            }
        }
    } else {
        None
    };*/
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
if let Some(mime_ty) = data_type {
if mime_ty.subtype() == mime::HTML {
let scheme = url.scheme();
let url_host_str = url.host_str().unwrap();
let base_url = {
if url_host_str.starts_with("//") {
format!("{}:{}", scheme, url_host_str).parse().unwrap()
} else {
format!("{}://{}", scheme, url_host_str).parse().unwrap()
}
};
tokio::spawn(|| {
let searcher = UrlSearcher {
crawl_url: url,
base_url,
data: resp,
};

});
}
}
}
}*/

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

/*async fn fetch(urls: Vec<String>) {
    let orchestrator = Orchestrator::new(urls);
    let future = orchestrator.crawl_urls();
    await!(future);
}*/

fn main() {
    /*let future = fetch(vec!["http://eagantigua.org/page563.html".to_string()]);
    let compat_future = future.boxed().unit_error().compat();
    tokio::run(compat_future);*/
}
