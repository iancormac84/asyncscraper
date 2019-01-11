#![feature(async_await, await_macro, futures_api, generators, gen_future)]

use {
    bytes::Bytes,
    futures::{channel::mpsc, compat::Future01CompatExt},
    hyper::{
        self,
        client::{connect::dns::TokioThreadpoolGaiResolver, HttpConnector},
        header::CONTENT_TYPE,
        Body, Client, Request, StatusCode,
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
            atomic::{AtomicUsize, Ordering},
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
    pub fn new() -> UrlPool {
        UrlPool {
            pool: VecDeque::new(),
        }
    }
    pub fn add_urls(&mut self, urls: Vec<Url>) {
        self.pool.extend(urls);
    }
    pub fn add_url(&mut self, url: Url) {
        self.pool.push_back(url);
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

struct Inner {
    inflight: AtomicUsize,
}

struct UrlStream {
    source: UrlPool,
    urls_in_flight: Arc<Inner>, //TODO: Share this with other data structures? Probably should, because the seed URLs are put into the UrlPool...or should they be?
    waker: Option<Waker>,
    urls_to_add_receiver: mpsc::UnboundedReceiver<Urls>,
}

impl UrlStream {
    pub fn new(
        seed: Vec<Url>,
        urls_to_add_receiver: mpsc::UnboundedReceiver<Urls>,
        urls_in_flight: Arc<Inner>,
    ) -> UrlStream {
        UrlStream {
            source: {
                let mut url_pool = UrlPool::new();
                url_pool.add_urls(seed);
                url_pool
            },
            urls_in_flight,
            waker: None,
            urls_to_add_receiver,
        }
    }
}

impl futures::stream::Stream for UrlStream {
    type Item = Url;

    fn poll_next(mut self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Option<Self::Item>> {
        use crate::Urls::*;
        let this = &mut *self;

        match this.source.try_get_url() {
            Ok(url) => {
                this.urls_in_flight.inflight.fetch_add(1, Ordering::SeqCst);
                Poll::Ready(Some(url))
            }
            Err(_) => {
                loop {
                    match Pin::new(&mut this.urls_to_add_receiver).poll_next(lw) {
                        Poll::Ready(Some(urls)) => match urls {
                            One(url) => this.source.add_url(url),
                            Multi(urls) => this.source.add_urls(urls),
                        },
                        Poll::Ready(None) => break,
                        Poll::Pending => return Poll::Pending,
                    }
                }
                if this.urls_in_flight.inflight.load(Ordering::SeqCst) == 0 {
                    Poll::Ready(None)
                } else {
                    this.waker = Some(lw.clone().into_waker());
                    Poll::Pending
                }
            }
        }
    }
}

pub enum Data {
    Html {
        crawl_data: CrawlData,
        base_url: Url,
    },
    Other {
        crawl_data: CrawlData,
        base_url: Url,
    },
}

pub struct UrlResourcePool {
    pub data_pool: Slab<CrawlData>,
    pub crawl_data_receiver: mpsc::UnboundedReceiver<CrawlData>,
    waker: Option<Waker>,
}

impl UrlResourcePool {
    pub fn new(crawl_data_receiver: mpsc::UnboundedReceiver<CrawlData>) -> UrlResourcePool {
        Self {
            data_pool: Slab::new(),
            crawl_data_receiver,
            waker: None,
        }
    }
}

impl futures::stream::Stream for UrlResourcePool {
    type Item = Data;

    fn poll_next(mut self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            if this.data_pool.is_empty() {
                //This loop drains the receiver and then breaks out of it because of the return statements.
                loop {
                    match Pin::new(&mut this.crawl_data_receiver).poll_next(lw) {
                        Poll::Ready(Some(crawl_data)) => {
                            this.data_pool.insert(crawl_data);
                        }
                        Poll::Ready(None) => return Poll::Ready(None),
                        Poll::Pending => return Poll::Pending,
                    }
                }
            }
            //Why no 'break' for this outer loop? Because the code below drains the data_pool, which will then invoke the inner loop above.
            let data = this.data_pool.remove(0);
            let scheme = data.source.scheme();
            let url_host_str = data.source.host_str().unwrap();
            let base_url = {
                if url_host_str.starts_with("//") {
                    format!("{}:{}", scheme, url_host_str).parse().unwrap()
                } else {
                    format!("{}://{}", scheme, url_host_str).parse().unwrap()
                }
            };
            if let Some(ref mime_ty) = data.data_type {
                if mime_ty.subtype() == mime::HTML {
                    return Poll::Ready(Some(Data::Html {
                        crawl_data: data,
                        base_url,
                    }));
                } else {
                    return Poll::Ready(Some(Data::Other {
                        crawl_data: data,
                        base_url,
                    }));
                }
            }
        }
    }
}

pub struct CrawlData {
    pub source: Arc<Url>,
    pub data_type: Option<Mime>,
    pub data: Bytes,
}

pub struct UrlStatus {
    pub url: Arc<Url>,
    pub error: Option<hyper::error::Error>,
    pub status_code: StatusCode,
}

pub struct UrlStatusHandler {
    inner: Arc<Inner>,
    url_status_receiver: mpsc::UnboundedReceiver<UrlStatus>,
    extracted_urls_sender: mpsc::UnboundedSender<UrlOpt>,
}

impl futures::stream::Stream for UrlStatusHandler {
    type Item = ();

    fn poll_next(mut self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            match Pin::new(&mut this.url_status_receiver).poll_next(lw) {
                Poll::Ready(Some(url_status)) => match url_status.error {
                    Some(error) => match url_status.status_code.as_u16() {
                        400..=599 => {
                            println!(
                                "Visit to {} was successful with a status code of {}.",
                                &url_status.url[..],
                                url.status_code.as_u16()
                            );
                            this.inner.inflight.fetch_sub(1, Ordering::SeqCst);
                            if let Err(err) = this.extracted_urls_sender.unbounded_send(UrlOpt One {
        url: url_status.url,
        url_opt_source: UrlOptSource::UrlStatusHandler,
    }) {
                                if err.is_disconnected() {
                                    panic!("URL status sender found disconnection error...ah wa de...!");
                                } else if err.is_full() {
                                    panic!("How yuh can be unbounded and be full???");
                                }
                            }
                        }
                        _ => println!("Saw status code {}", url_status.status_code.as_u16()),
                    },
                    None => {
                        println!(
                            "Visit to {} was successful with a status code of {}.",
                            &url_status.url[..],
                            url.status_code.as_u16()
                        );
                        this.inner.inflight.fetch_sub(1, Ordering::SeqCst);
                    }
                },
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
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
                let url_status = UrlStatus {
                    url: url.clone(),
                    error: Some(error),
                };
                if let Err(err) = url_status_sender.unbounded_send(url_status) {
                    if err.is_disconnected() {
                        panic!("URL status sender found disconnection error...ah wa de...!");
                    } else if err.is_full() {
                        panic!("How yuh can be unbounded and be full???");
                    }
                }
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
            Ok(x) => {
                let url_status = UrlStatus {
                    url: url.clone(),
                    error: None,
                };
                if let Err(err) = url_status_sender.unbounded_send(url_status) {
                    if err.is_disconnected() {
                        panic!("URL status sender found disconnection error...ah wa de...!");
                    } else if err.is_full() {
                        panic!("How yuh can be unbounded and be full???");
                    }
                }
                x.into()
            }
            Err(error) => {
                let url_status = UrlStatus {
                    url: url.clone(),
                    error: Some(error),
                };
                if let Err(err) = url_status_sender.unbounded_send(url_status) {
                    if err.is_disconnected() {
                        panic!("URL status sender found disconnection error...ah wa de...!");
                    } else if err.is_full() {
                        panic!("How yuh can be unbounded and be full???");
                    }
                }
                return;
            }
        }
    };
    let crawl_data = CrawlData {
        source: url.clone(),
        data_type,
        data,
    };
    if let Err(err) = crawl_data_sender.unbounded_send(crawl_data) {
        if err.is_disconnected() {
            panic!("Crawl data sender found disconnection error...ah wa de...!");
        } else if err.is_full() {
            panic!("How yuh can be unbounded and be full???");
        }
    }
}

async fn crawl_html(
    crawl_data: CrawlData,
    base_url: Url,
    extracted_urls_sender: mpsc::UnboundedSender<HashSet<Url>>,
) {
    let body = str::from_utf8(&crawl_data.data[..]).unwrap();
    let parser = kuchiki::parse_html().one(&body[..]);
    let link_selector = parser.select("a").unwrap();
    let mut urls = HashSet::new();
    for found_link in link_selector {
        let link_attribs = &(&*found_link).attributes;
        let link_attribs_borrow = link_attribs.borrow();
        let url = if let Some(url) = link_attribs_borrow.get("href") {
            match Url::parse(url) {
                Err(error) if error == url::ParseError::RelativeUrlWithoutBase => {
                    base_url.join(url).unwrap()
                }
                Err(error) => {
                    println!("Ignoring uri error {:?}. Discarding uri {:?}.", error, url);
                    continue;
                }
                Ok(url) => {
                    let host = url.host_str();
                    if let Some(host) = host {
                        if host != &base_url[..] {
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
    if let Err(err) = extracted_urls_sender.unbounded_send(urls) {
        if err.is_disconnected() {
            panic!("Crawl data sender found disconnection error...ah wa de...!");
        } else if err.is_full() {
            panic!("How yuh can be unbounded and be full???");
        }
    }
}

pub enum UrlOpt {
    One {
        url: Url,
        url_opt_source: UrlOptSource,
    },
    Set {
        urls: HashSet<Url>,
    },
}

pub enum UrlOptSource {
    UrlStatusHandler,
    Crawler,
}

pub enum Urls {
    One(Url),
    Multi(Vec<Url>),
}

pub struct UrlFilter {
    filter: CuckooFilter<Url>,
    extracted_urls_receiver: mpsc::UnboundedReceiver<UrlOpt>,
    urls_for_pool_sender: mpsc::UnboundedSender<Urls>,
}

impl UrlFilter {
    pub fn new(
        extracted_urls_receiver: mpsc::UnboundedReceiver<UrlOpt>,
        urls_for_pool_sender: mpsc::UnboundedSender<Urls>,
    ) -> UrlFilter {
        Self {
            filter: CuckooFilter::<Url>::from_entries_per_index(100, 0.01, 8),
            extracted_urls_receiver,
            urls_for_pool_sender,
        }
    }
}

impl futures::stream::Stream for UrlFilter {
    type Item = ();
    fn poll_next(mut self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            match Pin::new(&mut this.extracted_urls_receiver).poll_next(lw) {
                Poll::Ready(Some(urls)) => {
                    let maybe_urls = {
                        match urls {
                            UrlOpt::One {
                                url,
                                url_opt_source,
                            } => {
                                match url_opt_source {
                                    //If it was from UrlStatusHandler, it's to be put into the cuckoo filter to signify that is has been crawled.
                                    UrlOptSource::UrlStatusHandler => {
                                        this.filter.insert(&url);
                                        None
                                    }
                                    //If it was from Crawler, check to see if it's already in the cuckoo filter, then go from there.
                                    UrlOptSource::Crawler => {
                                        if !this.filter.contains(&url) {
                                            Some(Urls::One(url))
                                        } else {
                                            None
                                        }
                                    }
                                }
                            }
                            UrlOpt::Set { mut urls } => {
                                urls.retain(|url| !this.filter.contains(&url));
                                Some(Urls::Multi(urls.into_iter().collect()))
                            }
                        }
                    };
                    if let Some(urls) = maybe_urls {
                        if let Err(err) = this.urls_for_pool_sender.unbounded_send(urls) {
                            if err.is_disconnected() {
                                panic!(
                                    "Crawl data sender found disconnection error...ah wa de...!"
                                );
                            } else if err.is_full() {
                                panic!("How yuh can be unbounded and be full???");
                            }
                        }
                        return Poll::Ready(Some(()));
                    } else {
                        return Poll::Pending;
                    }
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

pub struct Orchestrator {
    client: Arc<SClient>,
    url_resource_pool: Arc<UrlResourcePool>,
    crawl_data_sender: mpsc::UnboundedSender<CrawlData>,
    url_stream: Arc<UrlStream>,
    url_filter: Arc<UrlFilter>,
    extracted_urls_sender: mpsc::UnboundedSender<UrlOpt>,
    url_status_handler: Arc<UrlStatusHandler>,
    url_status_sender: mpsc::UnboundedSender<UrlStatus>,
}

impl Orchestrator {
    pub fn new(urls: Vec<String>) -> Orchestrator {
        let urls = urls
            .into_iter()
            .map(|url| Url::parse(&url[..]).unwrap())
            .collect();
        //crawl_data_sender gets sent into process_url function. crawl_data_receiver is inside UrlResourcePool.
        let (crawl_data_sender, crawl_data_receiver) = mpsc::unbounded::<CrawlData>();
        //urls_for_pool_sender is sent into UrlFilter. urls_to_add_receiver is inside UrlStream. This setup is pretty much a single-producer single-consumer queue, because there is only one UrlFilter and one UrlStream. Is it possible to make more?
        let (urls_for_pool_sender, urls_to_add_receiver) = mpsc::unbounded::<Urls>();
        //extracted_urls_sender gets sent into crawl_html function. extracted_urls_receiver is inside UrlFilter.
        let (extracted_urls_sender, extracted_urls_receiver) = mpsc::unbounded::<UrlOpt>();
        let (url_status_sender, url_status_receiver) = mpsc::unbounded::<UrlStatus>();
        let inner = Arc::new(Inner {
            inflight: AtomicUsize::new(0),
        });
        let url_stream = Arc::new(UrlStream::new(
            urls,
            urls_to_add_receiver,
            Arc::clone(&inner),
        ));
        let url_status_handler = Arc::new(UrlStatusHandler {
            inner,
            url_status_receiver,
            extracted_urls_sender: extracted_urls_sender.clone(),
        });
        Orchestrator {
            client: {
                let tls_connector = TlsConnector::new().unwrap();
                let connector = HttpConnector::new_with_tokio_threadpool_resolver();
                let mut connector = HttpsConnector::from((connector, tls_connector));
                connector.https_only(false);
                Arc::new(Client::builder().keep_alive(true).build(connector))
            },
            url_resource_pool: Arc::new(UrlResourcePool::new(crawl_data_receiver)),
            crawl_data_sender,
            url_stream,
            url_filter: Arc::new(UrlFilter::new(
                extracted_urls_receiver,
                urls_for_pool_sender,
            )),
            extracted_urls_sender,
            url_status_handler,
            url_status_sender,
        }
    }
    pub fn run(&self) {
        /*tokio::spawn_async(
            async move { await!(crawl_html(data, base_url, extracted_urls_sender.clone())) },
        );*/
    }
}

fn main() {}
