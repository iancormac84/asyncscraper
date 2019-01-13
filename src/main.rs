#![feature(async_await, await_macro, futures_api, generators, gen_future)]

use {
    bytes::Bytes,
    futures::{channel::mpsc, compat::Future01CompatExt},
    hyper::{
        self,
        client::{connect::dns::TokioThreadpoolGaiResolver, HttpConnector},
        header::CONTENT_TYPE,
        Body, Client, Request,
    },
    hyper_tls::HttpsConnector,
    kuchiki::traits::TendrilSink,
    mime::Mime,
    native_tls::TlsConnector,
    pin_utils::pin_mut,
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
    pub fn len(&self) -> usize {
        self.pool.len()
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

struct InflightUrls {
    count: AtomicUsize,
}

struct InflightPayloads {
    count: AtomicUsize,
}

struct UrlPackets {
    count: AtomicUsize,
}

struct UrlStream {
    source: UrlPool,
    urls_in_flight: Arc<InflightUrls>,
    payloads_in_flight: Arc<InflightPayloads>,
    incoming_url_packets: Arc<UrlPackets>,
    waker: Option<Waker>,
    urls_to_add_receiver: mpsc::UnboundedReceiver<Urls>,
}

impl UrlStream {
    pub fn new(
        seed: Vec<Url>,
        urls_to_add_receiver: mpsc::UnboundedReceiver<Urls>,
        urls_in_flight: Arc<InflightUrls>,
        payloads_in_flight: Arc<InflightPayloads>,
        incoming_url_packets: Arc<UrlPackets>,
    ) -> UrlStream {
        UrlStream {
            source: {
                let mut url_pool = UrlPool::new();
                println!("url_pool.len() is {}.", url_pool.len());
                url_pool.add_urls(seed);
                println!("url_pool.len() is now {}.", url_pool.len());
                url_pool
            },
            urls_in_flight,
            payloads_in_flight,
            waker: None,
            urls_to_add_receiver,
            incoming_url_packets,
        }
    }
}

impl futures::stream::Stream for UrlStream {
    type Item = Url;

    fn poll_next(mut self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Option<Self::Item>> {
        use crate::Urls::*;

        println!("Inside UrlStream.");
        let this = &mut *self;
        println!(
            "Inside UrlStream, this.source.len() is {}.",
            this.source.len()
        );

        if this.urls_in_flight.count.load(Ordering::SeqCst) == 0
            && this.payloads_in_flight.count.load(Ordering::SeqCst) == 0
            && this.source.try_get_url().is_err()
        {
            println!("Inside UrlStream, found that shit.");
            return Poll::Ready(None);
        } else if this.incoming_url_packets.count.load(Ordering::SeqCst) != 0 {
            loop {
                match Pin::new(&mut this.urls_to_add_receiver).poll_next(lw) {
                    Poll::Ready(Some(urls)) => {
                        this.incoming_url_packets
                            .count
                            .fetch_sub(1, Ordering::SeqCst);
                        println!("Inside UrlStream, found Poll::Ready(Some({:?})), I guess we gon' loop again!", &urls);
                        match urls {
                            One(url) => this.source.add_url(url),
                            Multi(urls) => this.source.add_urls(urls),
                        }
                    }
                    Poll::Ready(None) | Poll::Pending => {
                        this.waker = Some(lw.clone().into_waker());
                        println!("Inside UrlStream, found Poll::Pending or Poll::Ready(None), about to break.");
                        break;
                    }
                }
            }
            match this.source.try_get_url() {
                Ok(url) => {
                    this.urls_in_flight.count.fetch_add(1, Ordering::SeqCst);
                    println!("About to return Poll::Ready(Some({})).", &url[..]);
                    return Poll::Ready(Some(url));
                }
                Err(_) => panic!("This shouldn't happen! We should have URLs to process!"),
            }
        } else {
            println!("this.source.len() is {}", this.source.len());
            match this.source.try_get_url() {
                Ok(url) => {
                    this.urls_in_flight.count.fetch_add(1, Ordering::SeqCst);
                    println!("About to return Poll::Ready(Some({})).", &url[..]);
                    return Poll::Ready(Some(url));
                }
                Err(_) => {
                    if this.urls_in_flight.count.load(Ordering::SeqCst) == 0 {
                        println!("Inside UrlStream, about to return Poll::Ready(None) after finding that there are no in-flight URLs.");
                        return Poll::Ready(None);
                    } else {
                        println!("Inside UrlStream, about to return Poll::Pending, because there are in-flight URLs, but no incoming URLs at the moment.");
                        this.waker = Some(lw.clone().into_waker());
                        return Poll::Pending;
                    }
                }
            }
        }
    }
}

pub enum Data {
    Html {
        url_payload: UrlPayload,
        base_url: Url,
    },
    Other {
        url_payload: UrlPayload,
        base_url: Url,
    },
}

struct UrlPayloadPool {
    pub url_payload_pool: Slab<UrlPayload>,
    pub url_payload_receiver: mpsc::UnboundedReceiver<UrlPayload>,
    waker: Option<Waker>,
    urls_in_flight: Arc<InflightUrls>,
    payloads_in_flight: Arc<InflightPayloads>,
}

impl UrlPayloadPool {
    pub fn new(
        url_payload_receiver: mpsc::UnboundedReceiver<UrlPayload>,
        urls_in_flight: Arc<InflightUrls>,
        payloads_in_flight: Arc<InflightPayloads>,
    ) -> UrlPayloadPool {
        Self {
            url_payload_pool: Slab::new(),
            url_payload_receiver,
            waker: None,
            urls_in_flight,
            payloads_in_flight,
        }
    }
}

impl futures::stream::Stream for UrlPayloadPool {
    type Item = Data;

    //TODO: When you reach back home, you have to rewrite this method so that it returns more granular Poll variants.
    //urls == 0 && payloads == 0 && payload_pool == 0 -> Poll::Ready(None)
    //urls == 0 && payloads != 0 -> pull from payload_receiver until it returns Poll::Pending (or Poll::Ready(None)?)
    //following from above option, payloads should be 0, but payload_pool != 0
    fn poll_next(mut self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Option<Self::Item>> {
        println!("Inside UrlPayloadPool.");
        let this = &mut *self;
        if this.urls_in_flight.count.load(Ordering::SeqCst) == 0
            && this.payloads_in_flight.count.load(Ordering::SeqCst) == 0
            && this.url_payload_pool.is_empty()
        {
            return Poll::Ready(None);
        } else if this.payloads_in_flight.count.load(Ordering::SeqCst) != 0 {
            //This condition should cover when payload pool is empty or not. It can be filled, because there are some payloads incoming through the channel.
            loop {
                println!("Inside inner loop of UrlPayloadPool.");
                match Pin::new(&mut this.url_payload_receiver).poll_next(lw) {
                    Poll::Ready(Some(url_payload)) => {
                        println!("Inserting url_payload.");
                        this.url_payload_pool.insert(url_payload);
                        this.payloads_in_flight.count.fetch_sub(1, Ordering::SeqCst);
                    }
                    Poll::Ready(None) | Poll::Pending => {
                        println!(
                            "Inside inner loop of UrlPayloadPool, breaking out of inner loop."
                        );
                        break;
                    }
                };
            }
            let url_payload = this.url_payload_pool.remove(0);
            let scheme = url_payload.source.scheme();
            let url_host_str = url_payload.source.host_str().unwrap();
            let base_url: Url = {
                if url_host_str.starts_with("//") {
                    format!("{}:{}", scheme, url_host_str).parse().unwrap()
                } else {
                    format!("{}://{}", scheme, url_host_str).parse().unwrap()
                }
            };
            println!("base_url is {}", &base_url[..]);
            if let Some(ref mime_ty) = url_payload.url_payload_type {
                if mime_ty.subtype() == mime::HTML {
                    return Poll::Ready(Some(Data::Html {
                        url_payload,
                        base_url,
                    }));
                } else {
                    return Poll::Ready(Some(Data::Other {
                        url_payload,
                        base_url,
                    }));
                }
            } else {
                return Poll::Ready(Some(Data::Other {
                    url_payload,
                    base_url,
                }));
            }
        } else {
            if this.url_payload_pool.is_empty() {
                this.waker = Some(lw.clone().into_waker());
                return Poll::Pending;
            } else {
                let url_payload = this.url_payload_pool.remove(0);
                let scheme = url_payload.source.scheme();
                let url_host_str = url_payload.source.host_str().unwrap();
                let base_url: Url = {
                    if url_host_str.starts_with("//") {
                        format!("{}:{}", scheme, url_host_str).parse().unwrap()
                    } else {
                        format!("{}://{}", scheme, url_host_str).parse().unwrap()
                    }
                };
                println!("base_url is {}", &base_url[..]);
                if let Some(ref mime_ty) = url_payload.url_payload_type {
                    if mime_ty.subtype() == mime::HTML {
                        return Poll::Ready(Some(Data::Html {
                            url_payload,
                            base_url,
                        }));
                    } else {
                        return Poll::Ready(Some(Data::Other {
                            url_payload,
                            base_url,
                        }));
                    }
                } else {
                    return Poll::Ready(Some(Data::Other {
                        url_payload,
                        base_url,
                    }));
                }
            }
        }
    }
}

pub struct UrlPayload {
    pub source: Arc<Url>,
    pub url_payload_type: Option<Mime>,
    pub url_payload: Bytes,
}

pub struct UrlStatus {
    pub url: Arc<Url>,
    pub error: Option<hyper::error::Error>,
}

pub struct UrlStatusHandler {
    urls_in_flight: Arc<InflightUrls>,
    url_status_receiver: mpsc::UnboundedReceiver<UrlStatus>,
    extracted_urls_sender: Arc<mpsc::UnboundedSender<UrlOpt>>,
}

impl futures::stream::Stream for UrlStatusHandler {
    type Item = ();

    fn poll_next(mut self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Option<Self::Item>> {
        println!("Inside UrlStatusHandler.");
        let this = &mut *self;
        loop {
            match Pin::new(&mut this.url_status_receiver).poll_next(lw) {
                Poll::Ready(Some(url_status)) => match url_status.error {
                    Some(error) => {
                        println!(
                            "Visit to {} was unsuccessful with error {}.",
                            &url_status.url[..],
                            error
                        );
                        println!("Inside UrlStatusHandler, about to decrement the number of inflight URLs by 1.");
                        this.urls_in_flight.count.fetch_sub(1, Ordering::SeqCst);
                        if let Err(err) = this.extracted_urls_sender.unbounded_send(UrlOpt::One {
                            url: url_status.url,
                            url_opt_source: UrlOptSource::UrlStatusHandler,
                        }) {
                            if err.is_disconnected() {
                                panic!(
                                    "URL status sender found disconnection error...ah wa de...!"
                                );
                            } else if err.is_full() {
                                panic!("How yuh can be unbounded and be full???");
                            }
                        }
                    }
                    None => {
                        println!("Visit to {} was successful.", &url_status.url[..]);
                        println!("Inside UrlStatusHandler, after finding no errors, about to decrement the number of inflight URLs by 1.");
                        this.urls_in_flight.count.fetch_sub(1, Ordering::SeqCst);
                        println!(
                            "Number of in-flight URLs is now {}",
                            this.urls_in_flight.count.load(Ordering::SeqCst)
                        );
                    }
                },
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

async fn process_url(
    payloads_in_flight: Arc<InflightPayloads>,
    url: Arc<Url>,
    client: Arc<SClient>,
    url_payload_sender: mpsc::UnboundedSender<UrlPayload>,
    url_status_sender: mpsc::UnboundedSender<UrlStatus>,
) {
    println!("Inside process_url().");
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
    let url_payload_type: Option<Mime> = if let Some(ct) = headers.get(CONTENT_TYPE).cloned() {
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
    let url_payload: Bytes = {
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
    let url_payload = UrlPayload {
        source: url.clone(),
        url_payload_type,
        url_payload,
    };
    if let Err(err) = url_payload_sender.unbounded_send(url_payload) {
        if err.is_disconnected() {
            panic!("url_payload_sender found disconnection error...ah wa de...!");
        } else if err.is_full() {
            panic!("How yuh can be unbounded and be full???");
        }
    } else {
        payloads_in_flight.count.fetch_add(1, Ordering::SeqCst);
    }
}

async fn crawl_html(
    url_payload: UrlPayload,
    base_url: Url,
    extracted_urls_sender: Arc<mpsc::UnboundedSender<UrlOpt>>,
) {
    println!("Inside crawl_html().");
    let body = str::from_utf8(&url_payload.url_payload[..]).unwrap();
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
    if let Err(err) = extracted_urls_sender.unbounded_send(UrlOpt::Set { urls }) {
        if err.is_disconnected() {
            panic!("extracted_urls_sender found disconnection error...ah wa de...!");
        } else if err.is_full() {
            panic!("How yuh can be unbounded and be full???");
        }
    }
}

#[derive(Clone, Debug)]
pub enum UrlOpt {
    One {
        url: Arc<Url>,
        url_opt_source: UrlOptSource,
    },
    Set {
        urls: HashSet<Url>,
    },
}

#[derive(Clone, Debug)]
pub enum UrlOptSource {
    UrlStatusHandler,
    Crawler,
}

#[derive(Clone, Debug)]
pub enum Urls {
    One(Url),
    Multi(Vec<Url>),
}

struct UrlFilter {
    filter: CuckooFilter<Url>,
    extracted_urls_receiver: mpsc::UnboundedReceiver<UrlOpt>,
    urls_for_pool_sender: mpsc::UnboundedSender<Urls>,
    url_packets: Arc<UrlPackets>,
}

impl UrlFilter {
    pub fn new(
        extracted_urls_receiver: mpsc::UnboundedReceiver<UrlOpt>,
        urls_for_pool_sender: mpsc::UnboundedSender<Urls>,
        url_packets: Arc<UrlPackets>,
    ) -> UrlFilter {
        Self {
            filter: CuckooFilter::<Url>::from_entries_per_index(100, 0.01, 8),
            extracted_urls_receiver,
            urls_for_pool_sender,
            url_packets,
        }
    }
}

impl futures::stream::Stream for UrlFilter {
    type Item = ();
    fn poll_next(mut self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Option<Self::Item>> {
        println!("Inside UrlFilter.");
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
                                            let url = Arc::try_unwrap(url).unwrap();
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
                                    "urls_for_pool_sender found disconnection error...ah wa de...!"
                                );
                            } else if err.is_full() {
                                panic!("How yuh can be unbounded and be full???");
                            }
                        }
                        this.url_packets.count.fetch_add(1, Ordering::SeqCst);
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

pub async fn crawl_urls(urls: Vec<String>) {
    use futures::stream::StreamExt;

    let client: Arc<SClient> = {
        let tls_connector = TlsConnector::new().unwrap();
        let connector = HttpConnector::new_with_tokio_threadpool_resolver();
        let mut connector = HttpsConnector::from((connector, tls_connector));
        connector.https_only(false);
        Arc::new(Client::builder().keep_alive(true).build(connector))
    };

    let urls: Vec<Url> = urls
        .into_iter()
        .map(|url| Url::parse(&url[..]).unwrap())
        .collect();
    println!("urls.len() is {}", urls.len());

    //url_payload_sender gets sent into process_url function. url_payload_receiver is inside UrlPayloadPool.
    let (url_payload_sender, url_payload_receiver) = mpsc::unbounded::<UrlPayload>();
    //urls_for_pool_sender is sent into UrlFilter. urls_to_add_receiver is inside UrlStream. This setup is pretty much a single-producer single-consumer queue, because there is only one UrlFilter and one UrlStream. Is it possible to make more?
    let (urls_for_pool_sender, urls_to_add_receiver) = mpsc::unbounded::<Urls>();
    //extracted_urls_sender gets sent into crawl_html function. extracted_urls_receiver is inside UrlFilter.
    let (extracted_urls_sender, extracted_urls_receiver) = mpsc::unbounded::<UrlOpt>();
    let extracted_urls_sender = Arc::new(extracted_urls_sender);
    let (url_status_sender, url_status_receiver) = mpsc::unbounded::<UrlStatus>();

    let urls_in_flight = Arc::new(InflightUrls {
        count: AtomicUsize::new(0),
    });
    let payloads_in_flight = Arc::new(InflightPayloads {
        count: AtomicUsize::new(0),
    });
    let incoming_url_packets = Arc::new(UrlPackets {
        count: AtomicUsize::new(0),
    });
    let mut url_stream = UrlStream::new(
        urls,
        urls_to_add_receiver,
        Arc::clone(&urls_in_flight),
        Arc::clone(&payloads_in_flight),
        Arc::clone(&incoming_url_packets),
    );

    tokio_spawn(
        async move {
            let url_filter = UrlFilter::new(
                extracted_urls_receiver,
                urls_for_pool_sender,
                Arc::clone(&incoming_url_packets),
            );
            pin_mut!(url_filter);
            while let Some(()) = await!(url_filter.next()) {}
        },
    );

    let urls_in_flight1 = Arc::clone(&urls_in_flight);
    let mut url_status_handler = UrlStatusHandler {
        urls_in_flight: Arc::clone(&urls_in_flight1),
        url_status_receiver,
        extracted_urls_sender: Arc::clone(&extracted_urls_sender),
    };

    tokio_spawn(async move { while let Some(()) = await!(url_status_handler.next()) {} });

    let payloads_in_flight1 = Arc::clone(&payloads_in_flight);
    let payloads_in_flight2 = Arc::clone(&payloads_in_flight1);
    tokio_spawn(
        async move {
            let url_payload_pool = UrlPayloadPool::new(
                url_payload_receiver,
                Arc::clone(&urls_in_flight1),
                payloads_in_flight2,
            );
            pin_mut!(url_payload_pool);
            let extracted_urls_sender1 = Arc::clone(&extracted_urls_sender);
            while let Some(url_payload) = await!(url_payload_pool.next()) {
                let extracted_urls_sender2 = Arc::clone(&extracted_urls_sender1);
                tokio_spawn(
                    async move {
                        if let Data::Html {
                            url_payload,
                            base_url,
                        } = url_payload
                        {
                            await!(crawl_html(url_payload, base_url, extracted_urls_sender2));
                        }
                    },
                );
            }
        },
    );

    //pin_mut!(url_stream);
    while let Some(url) = await!(url_stream.next()) {
        let client_clone = client.clone();
        let url_payload_sender_clone = url_payload_sender.clone();
        let url_status_sender_clone = url_status_sender.clone();
        let payloads_in_flight2 = Arc::clone(&payloads_in_flight1);
        tokio_spawn(
            async {
                await!(process_url(
                    payloads_in_flight2,
                    Arc::new(url),
                    client_clone,
                    url_payload_sender_clone,
                    url_status_sender_clone
                ))
            },
        );
    }
}

fn tokio_spawn<F: std::future::Future<Output = ()> + Send + 'static>(future: F) {
    use futures::future::FutureExt;
    tokio::spawn(futures::compat::Compat::new(Box::pin(
        future.map(|()| -> Result<(), ()> { Ok(()) }),
    )));
}

fn tokio_run<F: std::future::Future<Output = ()> + Send + 'static>(future: F) {
    use futures::future::FutureExt;
    tokio::run(futures::compat::Compat::new(Box::pin(
        future.map(|()| -> Result<(), ()> { Ok(()) }),
    )));
}

fn main() {
    let urls = vec!["http://eagantigua.org/page563.html".to_string()];
    tokio_run(async { await!(crawl_urls(urls)) });
}
