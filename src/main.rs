use bytes::Bytes;
use failure::{Context, Error};
use futures::{future, stream::futures_unordered::FuturesUnordered, Async, Poll};
use hyper::{
    client::{connect::dns::TokioThreadpoolGaiResolver, Client, HttpConnector, ResponseFuture},
    header::{HeaderMap, HeaderValue, CONTENT_LENGTH, CONTENT_TYPE},
    rt::{self, Future, Stream},
    Body, Chunk, Request, Response,
};
use hyper_tls::HttpsConnector;
use kuchiki::traits::TendrilSink;
use mime::Mime;
use native_tls::TlsConnector;
use std::{collections::HashSet, fmt, path::Path};
use streamunordered::{StreamUnordered, StreamYield};
use url::Url;

static START_URL: &'static str = "http://eagantigua.org/page563.html";
static USER_AGENT: &'static str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/70.0.3538.77 Safari/537.36";

pub use failure::Error as GitMachineError;
pub type GitMachineResult<T> = std::result::Result<T, GitMachineError>;

pub trait GitMachineResultExt<T, E> {
    fn chain_err<F, D>(self, f: F) -> Result<T, Context<D>>
    where
        F: FnOnce() -> D,
        D: fmt::Display + Send + Sync + 'static;
}

impl<T, E> GitMachineResultExt<T, E> for Result<T, E>
where
    E: Into<Error>,
{
    fn chain_err<F, D>(self, f: F) -> Result<T, Context<D>>
    where
        F: FnOnce() -> D,
        D: fmt::Display + Send + Sync + 'static,
    {
        self.map_err(|failure| {
            let err = failure.into();
            let context = f();
            err.context(context)
        })
    }
}

pub struct Crawler {
    client: Client<HttpsConnector<HttpConnector<TokioThreadpoolGaiResolver>>>,
    response_futures: FuturesUnordered<TrackedResponseFuture>,
    tracked_body_streams: StreamUnordered<TrackedBody>,
    data_crawlers: StreamUnordered<CrawlData>,
    errors: Vec<GitMachineError>,
    visited: HashSet<Url>,
}

impl Crawler {
    pub fn new(seed_urls: &[&str]) -> Crawler {
        let tls_connector = TlsConnector::new().unwrap();
        let connector = HttpConnector::new_with_tokio_threadpool_resolver();
        let mut connector = HttpsConnector::from((connector, tls_connector));
        connector.https_only(false);
        let client = Client::builder().keep_alive(true).build(connector);
        let mut response_futures = FuturesUnordered::new();
        for seed in seed_urls {
            let url = Url::parse(seed).unwrap();
            let request = Request::builder()
                .method("GET")
                .uri(&url[..])
                .header("User-Agent", USER_AGENT)
                .body(Body::empty())
                .unwrap();
            let tracked_response_future = TrackedResponseFuture {
                url,
                response_future: client.request(request),
            };
            response_futures.push(tracked_response_future);
        }
        Crawler {
            client,
            response_futures,
            tracked_body_streams: StreamUnordered::new(),
            data_crawlers: StreamUnordered::new(),
            errors: Vec::new(),
            visited: HashSet::new(),
        }
    }
}

impl Future for Crawler {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        println!("We right at the top, just before the self.response_futures.poll() loop.");
        loop {
            match self.response_futures.poll() {
                Ok(Async::Ready(Some(response))) => {
                    println!("Ready with a response.");
                    println!("response is {:?}", response);

                    let len = self.tracked_body_streams.len();
                    println!("self.tracked_body_streams.len() is {}", len);
                    self.tracked_body_streams.push(TrackedBody::new(response));
                    println!(
                        "After inserting stream, self.tracked_body_streams.len() is now {}",
                        self.tracked_body_streams.len()
                    );
                }
                Ok(Async::Ready(None)) => break,
                Ok(Async::NotReady) => futures::task::current().notify(),
                Err(error) => {
                    println!("{}", error);
                    self.errors.push(error);
                }
            }
        }

        println!("We right at the middle, just before the self.tracked_body_streams.poll() loop.");
        loop {
            match self.tracked_body_streams.poll() {
                Ok(Async::Ready(Some((body_chunk, idx)))) => {
                    println!("Response body stream at index {} is ready", idx);
                    match body_chunk {
                        StreamYield::Item(chunk) => {
                            let source_stream: &TrackedBody =
                                self.tracked_body_streams.get(idx).unwrap();
                            match &source_stream.data_type {
                                Some(mime_type) if mime_type == &mime::TEXT_HTML => {
                                    let mut target = 0;
                                    match source_stream.processor_idx {
                                        Some(index) => {
                                            let crawl_data: &mut CrawlData = self.data_crawlers.get_mut(index).unwrap();
                                            println!("Inside self.tracked_body_streams loop, found index {}. Extending raw_data.", index);
                                            crawl_data.raw_data.extend(chunk.into_iter());
                                            println!(
                                                "crawl_data.raw_data.len() is now {}",
                                                crawl_data.raw_data.len()
                                            );
                                        }
                                        None => {
                                            let (url, content_length) = {
                                                (
                                                    source_stream.url.clone(),
                                                    source_stream.data_length,
                                                )
                                            };
                                            println!("Inside self.tracked_body_streams loop, creating CrawlData for body stream at index {}.", idx);
                                            let raw_data = chunk.into_bytes();
                                            let insert_idx = self.data_crawlers.push(CrawlData {
                                                url,
                                                raw_data,
                                                last_size: 0,
                                                content_length,
                                                valid_data: String::new(),
                                                to_send: HashSet::new(),
                                                links: HashSet::new(),
                                            });
                                            target = insert_idx;
                                        }
                                    }
                                    let mutable_source_stream: &mut TrackedBody = self.tracked_body_streams.get_mut(idx).unwrap();
                                    mutable_source_stream.processor_idx = Some(target);
                                }
                                Some(mime_type) => {
                                    println!(
                                        "Found mime type {}. Will add code to process that later.",
                                        mime_type
                                    );
                                }
                                None => {
                                    println!("No mime type found for this stream {}.", idx);
                                }
                            }
                        }
                        StreamYield::Finished(_) => {
                            println!("Inside the finished part of the body stream poll. We got back the response body stream. Using it to delete its processer. Or not...what should I do?");
                            println!(
                                "self.response_futures.len() is {}",
                                self.response_futures.len()
                            );
                            println!(
                                "self.tracked_body_streams.len() is now {}.",
                                self.tracked_body_streams.len()
                            );
                        }
                    }
                }
                Ok(Async::Ready(None)) => break,
                Ok(Async::NotReady) => futures::task::current().notify(),
                Err(error) => {
                    println!("{}", error);
                    self.errors.push(error);
                }
            }
        }

        println!("We right at the end, just before the self.data_crawlers.poll() loop.");
        loop {
            println!("self.data_crawlers.len() is {}", self.data_crawlers.len());
            match self.data_crawlers.poll() {
                Ok(Async::Ready(Some((uri_stream, uri_stream_idx)))) => {
                    println!("Got some urls. Extending self.to_visit.");
                    println!("Uri stream at index {} is ready", uri_stream_idx);
                    match uri_stream {
                        StreamYield::Item(uri_list) => {
                            for uri in uri_list.into_iter() {
                                if !self.visited.contains(&uri) {
                                    let request = Request::builder()
                                        .method("GET")
                                        .uri(&uri[..])
                                        .header("User-Agent", USER_AGENT)
                                        .body(Body::empty())
                                        .unwrap();

                                    let response_future = self.client.request(request);

                                    let tracked_response = TrackedResponseFuture {
                                        url: uri.clone(),
                                        response_future,
                                    };

                                    self.visited.insert(uri);

                                    self.response_futures.push(tracked_response);
                                }
                            }

                            println!(
                                "self.response_futures.len() is {}",
                                self.response_futures.len()
                            );
                        }
                        StreamYield::Finished(_) => {
                            println!("Inside the finished part of the URI stream. We got back the uri stream.");
                        }
                    }
                }
                Ok(Async::Ready(None)) => {
                    println!("We are at Ok(Async::Ready(None)) for self.data_crawlers polling.");
                    break;
                }
                Ok(Async::NotReady) => futures::task::current().notify(),
                Err(error) => {
                    println!("{}", error);
                    self.errors.push(error);
                }
            }
        }

        if self.response_futures.is_empty()
            && self.tracked_body_streams.is_empty()
            && self.data_crawlers.is_empty()
        {
            println!("About to return Ok(Async::Ready(())).");
            Ok(Async::Ready(()))
        } else {
            println!("About to return Ok(Async::NotReady).");
            futures::task::current().notify();
            Ok(Async::NotReady)
        }
    }
}

pub struct TrackedResponseFuture {
    pub url: Url,
    pub response_future: ResponseFuture,
}

impl Future for TrackedResponseFuture {
    type Item = TrackedResponse;
    type Error = GitMachineError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.response_future.poll() {
            Ok(Async::Ready(response)) => Ok(Async::Ready(TrackedResponse {
                url: self.url.clone(),
                response,
            })),
            Ok(Async::NotReady) => {
                futures::task::current().notify();
                Ok(Async::NotReady)
            }
            Err(err) => Err(err.into()),
        }
    }
}

#[derive(Debug)]
pub struct TrackedResponse {
    pub url: Url,
    pub response: Response<Body>,
}

impl TrackedResponse {
    pub fn headers(&self) -> &HeaderMap<HeaderValue> {
        self.response.headers()
    }
    pub fn into_body(self) -> Body {
        self.response.into_body()
    }
}

pub struct TrackedBody {
    pub url: Url,
    pub data_length: Option<u64>,
    pub data_type: Option<Mime>,
    pub data: Body,
    pub processor_idx: Option<usize>,
}

impl TrackedBody {
    pub fn new(response: TrackedResponse) -> TrackedBody {
        let headers = response.headers();
        let data_length: Option<u64> = if let Some(cl) = headers.get(CONTENT_LENGTH).cloned() {
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
        };
        let data_type: Option<Mime> = if let Some(ct) = headers.get(CONTENT_TYPE).cloned() {
            let mime_str = ct.to_str();
            if mime_str.is_err() {
                None
            } else {
                let mime = mime_str.unwrap().parse::<Mime>();
                if mime.is_err() {
                    let ending = response.url.path_segments().unwrap();
                    let ending = ending.last().unwrap();
                    mime_guess::guess_mime_type_opt(Path::new(ending))
                } else {
                    Some(mime.unwrap())
                }
            }
        } else {
            None
        };

        TrackedBody {
            url: {
                println!("url moved from response.url is {:?}", &response.url[..]);
                response.url.clone()
            },
            data_length,
            data_type,
            data: response.into_body(),
            processor_idx: None,
        }
    }
}

impl Stream for TrackedBody {
    type Item = Chunk;
    type Error = GitMachineError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.data.poll() {
            Ok(Async::Ready(chunk)) => Ok(Async::Ready(chunk)),
            Ok(Async::NotReady) => {
                futures::task::current().notify();
                Ok(Async::NotReady)
            }
            Err(err) => Err(err.into()),
        }
    }
}

pub struct CrawlData {
    pub url: Url,
    pub raw_data: Bytes,
    pub last_size: usize,
    pub content_length: Option<u64>,
    pub valid_data: String,
    to_send: HashSet<Url>,
    links: HashSet<Url>,
}

impl Stream for CrawlData {
    type Item = HashSet<Url>;
    type Error = GitMachineError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if !self.to_send.is_empty() {
            self.to_send.clear();
        }
        if self.raw_data.len() == self.last_size {
            futures::task::current().notify();
            return Ok(Async::NotReady);
        } else {
            let res = String::from_utf8(self.raw_data.slice(0, self.raw_data.len()).to_vec());
            let parser = match res {
                Err(e) => {
                    println!("Got an error after creating res.");
                    let utf8_error = e.utf8_error();
                    let valid_len = utf8_error.valid_up_to();
                    let split_to = self.raw_data.split_to(valid_len);
                    let to_vec = split_to.into_iter().collect::<Vec<u8>>();
                    let vstr = String::from_utf8(to_vec).unwrap();
                    self.valid_data.extend(vstr.chars());
                    self.last_size = self.raw_data.len();
                    kuchiki::parse_html().one(&self.valid_data[..])
                }
                Ok(vstr) => {
                    println!("Awwww res is fine!");
                    let vstr_len = vstr.len();
                    self.valid_data.extend(vstr.chars());
                    self.raw_data.split_to(vstr_len);
                    self.last_size = self.raw_data.len();
                    kuchiki::parse_html().one(&self.valid_data[..])
                }
            };
            let mut a_links = if let Ok(found_link) = parser.select("a") {
                println!("Inside a_links and successfully created selector for <a> tags.");
                found_link
            } else {
                println!("There was an error creating <a> tags, so returning Ok(Async::NotReady).");
                futures::task::current().notify();
                return Ok(Async::NotReady);
            };
            loop {
                match a_links.next() {
                    Some(found_link) => {
                        let link_attribs = &(&*found_link).attributes;
                        let link_attribs_borrow = link_attribs.borrow();
                        let url = if let Some(uri) = link_attribs_borrow.get("href") {
                            match Url::parse(uri) {
                                Err(error) if error == url::ParseError::RelativeUrlWithoutBase => {
                                    self.url.join(uri).unwrap()
                                }
                                Err(error) => {
                                    println!(
                                        "Ignoring uri error {:?}. Discarding uri {:?}.",
                                        error, uri
                                    );
                                    continue;
                                }
                                Ok(url) => {
                                    let host = url.host_str();
                                    if let Some(host) = host {
                                        if host != "eagantigua.org" {
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
                            println!("Couldn't find an URL attached to the href? Maybe the tag isn't closed.");
                            //Maybe this should be 'break;' instead, but if there's no more html to parse, it should end up in the None match after the subsequent iteration of the loop.
                            continue;
                        };
                        if url != self.url && !self.links.contains(&url) {
                            println!("self.url is {}", &self.url[..]);
                            println!("self.content_length is {}", self.content_length.unwrap());
                            self.to_send.insert(url.clone());
                            self.links.insert(url);
                        };
                    }
                    None => {
                        println!("Did not find any <a> tags. Breaking.");
                        break;
                    }
                }
            }
            let mut iterations = 0;
            if self.to_send.is_empty() {
                let valid_data_len = self.valid_data.len() as u64;
                let content_length = self.content_length.unwrap();
                if valid_data_len == content_length {
                    println!(
                        "valid_data_len is {}, and self.content_length is {}",
                        valid_data_len, content_length
                    );
                    Ok(Async::Ready(None))
                } else {
                    println!("Returning Ok(Async::NotReady) because self.to_send is empty and valid_data_len ({}) != content_length ({}).", valid_data_len, content_length);
                    if valid_data_len > content_length {
                        iterations += 1;
                        if iterations <= 3 {
                            println!("self.valid_data is {}", self.valid_data);
                            println!("LOOK FE DE ITERATIONS!");
                        }
                    }
                    futures::task::current().notify();
                    Ok(Async::NotReady)
                }
            } else {
                println!("self.to_send is {:?}", self.to_send);
                Ok(Async::Ready(Some(self.to_send.clone())))
            }
        }
    }
}

fn main() {
    let mut crawler = Crawler::new(&[START_URL]);
    rt::run(future::poll_fn(move || loop {
        match crawler.poll() {
            Ok(Async::Ready(())) => break Ok(Async::Ready(())),
            _ => {}
        }
    }));
}
