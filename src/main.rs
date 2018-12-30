#![feature(async_await, await_macro, futures_api)]

use futures::compat::Future01CompatExt;
use futures::lock::Mutex;
use futures_util::future::FutureExt;
use futures_util::try_future::TryFutureExt;
use hyper::{
    client::{connect::dns::TokioThreadpoolGaiResolver, HttpConnector},
    header::CONTENT_TYPE,
    Body, Client, Request, Response,
};
use hyper_tls::HttpsConnector;
use kuchiki::traits::TendrilSink;
use mime::Mime;
use native_tls::TlsConnector;
use std::{collections::HashSet, path::Path, str, sync::Arc};
use tokio::prelude::*;
use url::Url;

type SClient = Client<HttpsConnector<HttpConnector<TokioThreadpoolGaiResolver>>>;
type AsyncUrlSet = Mutex<HashSet<Url>>;

pub struct Orchestrator {
    seed_urls: Vec<String>,
    client: Arc<SClient>,
    to_visit: Arc<AsyncUrlSet>,
    visited: Arc<AsyncUrlSet>,
}

impl Orchestrator {
    pub fn new(urls: Vec<String>) -> Orchestrator {
        Orchestrator {
            seed_urls: urls,
            client: {
                let tls_connector = TlsConnector::new().unwrap();
                let connector = HttpConnector::new_with_tokio_threadpool_resolver();
                let mut connector = HttpsConnector::from((connector, tls_connector));
                connector.https_only(false);
                Arc::new(Client::builder().keep_alive(true).build(connector))
            },
            to_visit: Arc::new(Mutex::new(HashSet::new())),
            visited: Arc::new(Mutex::new(HashSet::new())),
        }
    }
    pub async fn crawl_urls(&self) {
        for url in &self.seed_urls[..] {
            let url: Url = url.parse().unwrap();
            {
                let mut visited_acquired = await!(self.visited.lock());
                //TODO: What if the request failed? Maybe it would be better to process the
                //result of the client call to know whether the URL is active, and then proceed
                //accordingly. If the URL is dead, it needs to be removed from the list of seed
                //URLs. If it seems to be alive, but the request failed for some reason, one
                //may want to retry, so it shouldn't be removed from the list of seed URLs.
                //Maybe it should be put in a 'to_retry' list?
                visited_acquired.insert(url.clone());
                println!("At the top, visited_acquired is {:?}", &visited_acquired);
            }
            let request = Request::builder()
                .method("GET")
                .uri(&url[..])
                .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/71.0.3578.98 Safari/537.36")
                .body(Body::empty())
                .unwrap();
            await!(self.process_request(request, url.clone()));
            println!("Back at the top!");
        }
        let to_visit_acquired = await!(self.to_visit.lock());
        println!("{:?}", to_visit_acquired);
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
                await!(self.collect_html_data(UrlSearcher {
                    base_url,
                    data: resp,
                }));
            }
        }
    }

    async fn collect_html_data(&self, searcher: UrlSearcher) {
        let urls = await!(searcher.crawl_html());
        println!("urls is {} URLs long", urls.len());

        let to_send: Vec<Url> = {
            let visited_acquired = await!(self.visited.lock());
            println!("visited_acquired is {:?}", &visited_acquired);
            let to_send = urls.difference(&visited_acquired).cloned().collect();
            to_send
        };
        println!("to_send is {:?}", to_send);
        let mut to_visit_acquired = await!(self.to_visit.lock());
        for uri in to_send.into_iter() {
            to_visit_acquired.insert(uri);
        }
        println!("to_visit_acquired is {:?}", to_visit_acquired);
    }
}

pub struct UrlSearcher {
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

async fn fetch(urls: Vec<String>) {
    let orchestrator = Orchestrator::new(urls);
    let future = orchestrator.crawl_urls();
    await!(future);
}

fn main() {
    let future = fetch(vec!["http://eagantigua.org/page563.html".to_string()]);
    let compat_future = future.boxed().unit_error().compat();
    tokio::run(compat_future);
}
