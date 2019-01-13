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
    pub fn is_empty(&self) -> bool {
        self.pool.is_empty()
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

struct IncomingPayloads {
    count: AtomicUsize,
}

struct IncomingUrlPackets {
    count: AtomicUsize,
}

struct IncomingUrlStates {
    count: AtomicUsize,
}

struct UrlStream {
    source: UrlPool,
    urls_in_flight: Arc<InflightUrls>,
    incoming_payloads: Arc<IncomingPayloads>,
    incoming_url_packets: Arc<IncomingUrlPackets>,
    waker: Option<Waker>,
    urls_to_add_receiver: mpsc::UnboundedReceiver<Urls>,
}

impl UrlStream {
    pub fn new(
        seed: Vec<Url>,
        urls_to_add_receiver: mpsc::UnboundedReceiver<Urls>,
        urls_in_flight: Arc<InflightUrls>,
        incoming_payloads: Arc<IncomingPayloads>,
        incoming_url_packets: Arc<IncomingUrlPackets>,
    ) -> UrlStream {
        UrlStream {
            source: {
                let mut url_pool = UrlPool::new();
                url_pool.add_urls(seed);
                url_pool
            },
            urls_in_flight,
            incoming_payloads,
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

        let (no_incoming_packets, no_urls_in_flight, no_incoming_payloads, source_is_empty) = (
            this.incoming_url_packets.count.load(Ordering::SeqCst) == 0,
            this.urls_in_flight.count.load(Ordering::SeqCst) == 0,
            this.incoming_payloads.count.load(Ordering::SeqCst) == 0,
            this.source.is_empty(),
        );
        println!(
            "The quorum says {:?}",
            (
                no_incoming_packets,
                no_urls_in_flight,
                no_incoming_payloads,
                source_is_empty
            )
        );

        match (
            no_incoming_packets,
            no_urls_in_flight,
            no_incoming_payloads,
            source_is_empty,
        ) {
            (true, true, true, true) => {
                println!("Inside UrlStream, found that shit.");
                return Poll::Ready(None);
            }
            (true, false, true, true) | (true, true, false, true) | (true, false, false, true) => {
                println!("Inside UrlStream, gonna just adopt a waker, and go to sleep.");
                this.waker = Some(lw.clone().into_waker());
                return Poll::Pending;
            }
            _ => {
                if !no_incoming_packets {
                    println!("We got some incoming packets!");
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
                }
                println!("About to try to get a URL now.");
                println!("this.source.len() is {} at this stage.", this.source.len());
                match this.source.try_get_url() {
                    Ok(url) => {
                        this.urls_in_flight.count.fetch_add(1, Ordering::SeqCst);
                        println!("About to return Poll::Ready(Some({})).", &url[..]);
                        return Poll::Ready(Some(url));
                    }
                    Err(_) => panic!("This shouldn't happen! We should have URLs to process!"),
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
    url_payload_pool: Slab<UrlPayload>,
    url_payload_receiver: mpsc::UnboundedReceiver<UrlPayload>,
    waker: Option<Waker>,
    urls_in_flight: Arc<InflightUrls>,
    incoming_payloads: Arc<IncomingPayloads>,
}

impl UrlPayloadPool {
    pub fn new(
        url_payload_receiver: mpsc::UnboundedReceiver<UrlPayload>,
        urls_in_flight: Arc<InflightUrls>,
        incoming_payloads: Arc<IncomingPayloads>,
    ) -> UrlPayloadPool {
        Self {
            url_payload_pool: Slab::new(),
            url_payload_receiver,
            waker: None,
            urls_in_flight,
            incoming_payloads,
        }
    }
}

impl futures::stream::Stream for UrlPayloadPool {
    type Item = Data;

    fn poll_next(mut self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Option<Self::Item>> {
        println!("Inside UrlPayloadPool.");
        let this = &mut *self;

        let (no_urls_in_flight, no_incoming_payloads, is_url_payload_pool_empty) = (
            this.urls_in_flight.count.load(Ordering::SeqCst) == 0,
            this.incoming_payloads.count.load(Ordering::SeqCst) == 0,
            this.url_payload_pool.is_empty(),
        );
        match (
            no_urls_in_flight,
            no_incoming_payloads,
            is_url_payload_pool_empty,
        ) {
            (true, true, true) => return Poll::Ready(None),
            (false, true, true) => {
                this.waker = Some(lw.clone().into_waker());
                return Poll::Pending;
            }
            _ => {
                if !no_incoming_payloads {
                    loop {
                        println!("Inside inner loop of UrlPayloadPool.");
                        match Pin::new(&mut this.url_payload_receiver).poll_next(lw) {
                            Poll::Ready(Some(url_payload)) => {
                                println!("Inserting url_payload.");
                                this.url_payload_pool.insert(url_payload);
                                this.incoming_payloads.count.fetch_sub(1, Ordering::SeqCst);
                            }
                            Poll::Ready(None) | Poll::Pending => {
                                this.waker = Some(lw.clone().into_waker());
                                println!(
                                "Inside inner loop of UrlPayloadPool, breaking out of inner loop."
                            );
                                break;
                            }
                        };
                    }
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
            }
        }
    }
}

pub struct UrlPayload {
    pub source: Arc<Url>,
    pub url_payload_type: Option<Mime>,
    pub url_payload: Bytes,
}

pub struct UrlState {
    pub url: Arc<Url>,
    pub error: Option<hyper::error::Error>,
}

pub struct UrlStateHandler {
    urls_in_flight: Arc<InflightUrls>,
    incoming_url_states: Arc<IncomingUrlStates>,
    url_state_receiver: mpsc::UnboundedReceiver<UrlState>,
    extracted_urls_sender: Arc<mpsc::UnboundedSender<UrlOpt>>,
    waker: Option<Waker>,
    url_states_pool: Slab<UrlState>,
}

impl futures::stream::Stream for UrlStateHandler {
    type Item = ();

    fn poll_next(mut self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Option<Self::Item>> {
        println!("Inside UrlStateHandler.");
        let this = &mut *self;

        let (no_urls_in_flight, no_incoming_url_states, url_states_pool_is_empty) = (
            this.urls_in_flight.count.load(Ordering::SeqCst) == 0,
            this.incoming_url_states.count.load(Ordering::SeqCst) == 0,
            this.url_states_pool.is_empty(),
        );

        match (
            no_urls_in_flight,
            no_incoming_url_states,
            url_states_pool_is_empty,
        ) {
            (true, true, true) => return Poll::Ready(None),
            (false, true, true) => {
                this.waker = Some(lw.clone().into_waker());
                return Poll::Pending;
            }
            _ => {
                if !no_incoming_url_states {
                    loop {
                        match Pin::new(&mut this.url_state_receiver).poll_next(lw) {
                            Poll::Ready(Some(url_state)) => {
                                this.url_states_pool.insert(url_state);
                                this.incoming_url_states
                                    .count
                                    .fetch_sub(1, Ordering::SeqCst);
                                this.urls_in_flight.count.fetch_sub(1, Ordering::SeqCst);
                            }
                            Poll::Ready(None) | Poll::Pending => {
                                this.waker = Some(lw.clone().into_waker());
                                println!(
                                "Inside inner loop of UrlStateHandler, breaking out of inner loop."
                            );
                                break;
                            }
                        }
                    }
                }
                let url_state = this.url_states_pool.remove(0);
                match url_state.error {
                    Some(ref error) => {
                        println!(
                            "Visit to {} was unsuccessful with error {}.",
                            &url_state.url[..],
                            error
                        );
                        //<---- When I start to actually sift through errors to determine what URLs could be retried, this will become relevant again.
                        //println!("Inside UrlStateHandler, about to decrement the number of inflight URLs by 1.");
                        //this.urls_in_flight.count.fetch_sub(1, Ordering::SeqCst);
                        let message = UrlOpt::One {
                            url: Arc::clone(&url_state.url),
                            url_opt_source: UrlOptSource::UrlStateHandler,
                        };
                        match this.extracted_urls_sender.unbounded_send(message) {
                            Err(err) => {
                                if err.is_disconnected() {
                                    panic!("URL status sender found disconnection error...ah wa de...!");
                                } else if err.is_full() {
                                    this.url_states_pool.insert(url_state);
                                    this.waker = Some(lw.clone().into_waker());
                                    //Because there's URL states to be sent, dammit! We can discard the message because we will recreate it when the url_state comes back around for processing.
                                    return Poll::Pending;
                                } else {
                                    unreachable!("You have reached a unicorn error!");
                                }
                            }
                            Ok(()) => {
                                if this.urls_in_flight.count.load(Ordering::SeqCst) != 0 {
                                    this.waker = Some(lw.clone().into_waker());
                                    return Poll::Pending;
                                } else {
                                    return Poll::Ready(None);
                                }
                            }
                        }
                    }
                    None => {
                        println!("Visit to {} was successful.", &url_state.url[..]);
                        if this.urls_in_flight.count.load(Ordering::SeqCst) != 0 {
                            this.waker = Some(lw.clone().into_waker());
                            return Poll::Pending;
                        } else {
                            return Poll::Ready(None);
                        }
                        /*println!("Inside UrlStateHandler, after finding no errors, about to decrement the number of inflight URLs by 1.");
                        this.urls_in_flight.count.fetch_sub(1, Ordering::SeqCst);
                        println!(
                            "Number of in-flight URLs is now {}",
                            this.urls_in_flight.count.load(Ordering::SeqCst)
                        );*/
                    }
                }
            }
        }
    }
}

async fn process_url(
    incoming_payloads: Arc<IncomingPayloads>,
    url: Arc<Url>,
    client: Arc<SClient>,
    url_payload_sender: mpsc::UnboundedSender<UrlPayload>,
    url_state_sender: mpsc::UnboundedSender<UrlState>,
    incoming_url_states_counter: Arc<IncomingUrlStates>,
) {
    println!("Inside process_url().");
    let req = Request::builder()
            .method("GET")
            .uri(&url[..])
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/71.0.3578.98 Safari/537.36")
            .body(Body::empty())
            .unwrap();
    println!("Made my request!");
    let resp = {
        match await!(client.clone().request(req).compat()) {
            Ok(x) => x,
            Err(error) => {
                let url_state = UrlState {
                    url: url.clone(),
                    error: Some(error),
                };
                if let Err(err) = url_state_sender.unbounded_send(url_state) {
                    if err.is_disconnected() {
                        panic!("URL status sender found disconnection error...ah wa de...!");
                    } else if err.is_full() {
                        panic!("How yuh can be unbounded and be full???");
                    }
                }
                incoming_url_states_counter
                    .count
                    .fetch_add(1, Ordering::SeqCst);
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
                let url_state = UrlState {
                    url: url.clone(),
                    error: None,
                };
                if let Err(err) = url_state_sender.unbounded_send(url_state) {
                    if err.is_disconnected() {
                        panic!("URL status sender found disconnection error...ah wa de...!");
                    } else if err.is_full() {
                        panic!("How yuh can be unbounded and be full???");
                    }
                }
                incoming_url_states_counter
                    .count
                    .fetch_add(1, Ordering::SeqCst);
                x.into()
            }
            Err(error) => {
                let url_state = UrlState {
                    url: url.clone(),
                    error: Some(error),
                };
                if let Err(err) = url_state_sender.unbounded_send(url_state) {
                    if err.is_disconnected() {
                        panic!("URL status sender found disconnection error...ah wa de...!");
                    } else if err.is_full() {
                        panic!("How yuh can be unbounded and be full???");
                    }
                }
                incoming_url_states_counter
                    .count
                    .fetch_add(1, Ordering::SeqCst);
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
        incoming_payloads.count.fetch_add(1, Ordering::SeqCst);
        incoming_url_states_counter
            .count
            .fetch_add(1, Ordering::SeqCst);
    }
}

async fn crawl_html(
    url_payload: UrlPayload,
    base_url: Url,
    extracted_urls_sender: Arc<mpsc::UnboundedSender<UrlOpt>>,
) {
    println!("Inside crawl_html().");
    let body = str::from_utf8(&url_payload.url_payload[..]).unwrap();
    println!("body is {}", &body[..]);
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
    UrlStateHandler,
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
    url_packets: Arc<IncomingUrlPackets>,
    waker: Option<Waker>,
}

impl UrlFilter {
    pub fn new(
        extracted_urls_receiver: mpsc::UnboundedReceiver<UrlOpt>,
        urls_for_pool_sender: mpsc::UnboundedSender<Urls>,
        url_packets: Arc<IncomingUrlPackets>,
    ) -> UrlFilter {
        Self {
            filter: CuckooFilter::<Url>::from_entries_per_index(100, 0.01, 8),
            extracted_urls_receiver,
            urls_for_pool_sender,
            url_packets,
            waker: None,
        }
    }
}

impl futures::stream::Stream for UrlFilter {
    type Item = ();
    fn poll_next(mut self: Pin<&mut Self>, lw: &LocalWaker) -> Poll<Option<Self::Item>> {
        println!("Inside UrlFilter.");
        let this = &mut *self;

        match Pin::new(&mut this.extracted_urls_receiver).poll_next(lw) {
            Poll::Ready(Some(urls)) => {
                let maybe_urls = {
                    match urls {
                        UrlOpt::One {
                            url,
                            url_opt_source,
                        } => {
                            match url_opt_source {
                                //If it was from UrlStateHandler, it's to be put into the cuckoo filter to signify that is has been crawled.
                                UrlOptSource::UrlStateHandler => {
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
                            panic!("urls_for_pool_sender found disconnection error...ah wa de...!");
                        } else if err.is_full() {
                            panic!("How yuh can be unbounded and be full???");
                        }
                    }
                    this.url_packets.count.fetch_add(1, Ordering::SeqCst);
                    return Poll::Ready(Some(()));
                } else {
                    this.waker = Some(lw.clone().into_waker());
                    return Poll::Pending;
                }
            }
            Poll::Ready(None) => return Poll::Ready(None),
            Poll::Pending => {
                this.waker = Some(lw.clone().into_waker());
                return Poll::Pending;
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
    let (url_state_sender, url_state_receiver) = mpsc::unbounded::<UrlState>();

    let urls_in_flight = Arc::new(InflightUrls {
        count: AtomicUsize::new(0),
    });
    let incoming_payloads = Arc::new(IncomingPayloads {
        count: AtomicUsize::new(0),
    });
    let incoming_url_packets = Arc::new(IncomingUrlPackets {
        count: AtomicUsize::new(0),
    });
    let incoming_url_states = Arc::new(IncomingUrlStates {
        count: AtomicUsize::new(0),
    });
    let mut url_stream = UrlStream::new(
        urls,
        urls_to_add_receiver,
        Arc::clone(&urls_in_flight),
        Arc::clone(&incoming_payloads),
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
    let incoming_url_states1 = Arc::clone(&incoming_url_states);
    let mut url_state_handler = UrlStateHandler {
        urls_in_flight: Arc::clone(&urls_in_flight1),
        incoming_url_states: Arc::clone(&incoming_url_states1),
        url_state_receiver,
        extracted_urls_sender: Arc::clone(&extracted_urls_sender),
        waker: None,
        url_states_pool: Slab::new(),
    };

    tokio_spawn(async move { while let Some(()) = await!(url_state_handler.next()) {} });

    let incoming_payloads1 = Arc::clone(&incoming_payloads);
    let incoming_payloads2 = Arc::clone(&incoming_payloads);
    let urls_in_flight2 = Arc::clone(&urls_in_flight1);
    let extracted_urls_sender1 = Arc::clone(&extracted_urls_sender);
    let extracted_urls_sender2 = Arc::clone(&extracted_urls_sender1);
    tokio_spawn(
        async move {
            let url_payload_pool =
                UrlPayloadPool::new(url_payload_receiver, urls_in_flight2, incoming_payloads2);
            pin_mut!(url_payload_pool);
            let extracted_urls_sender3 = Arc::clone(&extracted_urls_sender2);
            while let Some(url_payload) = await!(url_payload_pool.next()) {
                let extracted_urls_sender4 = Arc::clone(&extracted_urls_sender3);
                tokio_spawn(
                    async move {
                        if let Data::Html {
                            url_payload,
                            base_url,
                        } = url_payload
                        {
                            await!(crawl_html(url_payload, base_url, extracted_urls_sender4));
                        }
                    },
                );
            }
        },
    );

    //pin_mut!(url_stream);
    while let Some(url) = await!(url_stream.next()) {
        println!("Got an URL {:?} inside the async while loop.", url);
        let client1 = client.clone();
        let url_payload_sender1 = url_payload_sender.clone();
        let url_state_sender1 = url_state_sender.clone();
        let incoming_payloads2 = Arc::clone(&incoming_payloads1);
        let incoming_url_states_counter = Arc::clone(&incoming_url_states1);
        tokio_spawn(
            async {
                await!(process_url(
                    incoming_payloads2,
                    Arc::new(url),
                    client1,
                    url_payload_sender1,
                    url_state_sender1,
                    incoming_url_states_counter
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
