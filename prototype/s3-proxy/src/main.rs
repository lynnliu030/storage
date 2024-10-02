use s3s::auth::SimpleAuth;
use s3s::service::S3ServiceBuilder;
use s3s::{S3Request, S3};

use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::info;
mod client_impls;
mod objstore_client;
mod skyproxy;
mod utils {
    pub mod stream_utils;
    pub mod type_utils;
}

use crate::skyproxy::SkyProxy;
use crate::skyproxy::SkyProxyConfig;
use crate::utils::type_utils::new_head_object_request;
use futures::future::FutureExt;
use s3s::S3Error;
use std::env;
use tower::Service;
pub mod skyproxy_test;

#[derive(serde::Deserialize)]
struct WarmupRequest {
    bucket: String,
    key: String,
    warmup_regions: Vec<String>,
}

#[tokio::main]
async fn main() {
    // Exit the process upon panic, this is used for debugging purpose.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_panic(info);
        std::process::exit(1);
    }));

    tracing_subscriber::fmt()
        .pretty()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        // .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .init();

    // Setup our proxy object
    let init_regions: Vec<String> = env::var("INIT_REGIONS")
        .map(|s| s.split(',').map(|s| s.to_string()).collect())
        .unwrap_or_else(|_| vec!["aws:us-east-1".to_string()]);
    let client_from_region: String =
        env::var("CLIENT_FROM_REGION").expect("CLIENT_FROM_REGION must be set");
    let local_run: bool = env::var("LOCAL")
        .map(|s| s.parse::<bool>().unwrap())
        .unwrap_or(true);
    let skystore_bucket_prefix: String =
        env::var("SKYSTORE_BUCKET_PREFIX").expect("SKYSTORE_BUCKET_PREFIX is missing");

    let local_server: bool = env::var("LOCAL_SERVER")
        .map(|s| s.parse::<bool>().unwrap())
        .unwrap_or(true);

    let server_addr: String = env::var("SERVER_ADDR").expect("SERVER ADDRESS is missing");

    let get_policy: String = env::var("GET_POLICY").expect("POLICY for data transfer must be set");

    let put_policy: String = env::var("PUT_POLICY").expect("POLICY for data placement must be set");

    let version_enable: String = env::var("VERSION_ENABLE").expect("VERSION_ENABLE must be set");

    let config = SkyProxyConfig {
        regions: init_regions.clone(),
        client_from_region: client_from_region.clone(),
        local: local_run,
        local_server,
        policy: (get_policy.clone(), put_policy.clone()),
        skystore_bucket_prefix: skystore_bucket_prefix.clone(),
        version_enable: version_enable.clone(),
        server_addr: server_addr.clone(),
    };

    let proxy = SkyProxy::new(config).await;

    // Setup S3 service
    // TODO: Add auth and configure virtual-host style domain
    // https://github.com/Nugine/s3s/blob/b0b6878dafee0e08a876bec5239425fc40c01271/crates/s3s-fs/src/main.rs#L58-L66

    let s3_service = {
        // let mut b = S3ServiceBuilder::new(proxy);
        let mut b = S3ServiceBuilder::new(proxy.clone());
        // Enable authentication
        let access_key = std::env::var("AWS_ACCESS_KEY_ID").expect("ACCESS_KEY must be set");
        let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").expect("SECRET_KEY must be set");
        b.set_auth(SimpleAuth::from_single(access_key, secret_key));
        b.build().into_shared()
    };

    let service = ServiceBuilder::new()
        .layer(
            TraceLayer::new_for_http()
                .on_request(|req: &hyper::Request<hyper::Body>, _span: &tracing::Span| {
                    info!("{} {} {:?}", req.method(), req.uri(), req.headers());
                })
                .on_response(
                    |res: &hyper::Response<s3s::Body>,
                     latency: std::time::Duration,
                     _span: &tracing::Span| {
                        info!(
                            "{} {}ms: {:?}",
                            res.status(),
                            latency.as_millis(),
                            res.body().bytes()
                        );
                    },
                ),
        )
        .service_fn(move |mut req: hyper::Request<hyper::Body>| {
            let mut s3_service = s3_service.clone();
            let proxy_clone = proxy.clone();

            // print get policy
            let put_policy = put_policy.clone();
            println!("PUT POLICY: {}", put_policy);

            if put_policy == "always_store"
                || put_policy == "tevict"
                || put_policy == "optimal"
                || put_policy == "teven"
                || put_policy == "fixedttl"
                || put_policy == "ewma"
            {
                // print a log
                println!("INSERT HEADER: X-SKYSTORE-PULL: {}", put_policy);

                req.headers_mut()
                    .insert("X-SKYSTORE-PULL", put_policy.parse().unwrap());
            }

            if req.uri().path() == "/_/warmup_object" {
                let fut = async move {
                    if let Ok(body) = hyper::body::to_bytes(req.into_body()).await {
                        let warmup_req: WarmupRequest = serde_json::from_slice(&body).unwrap();

                        let head_object_input = new_head_object_request(
                            warmup_req.bucket.clone(),
                            warmup_req.key.clone(),
                            None, // version_id
                        );

                        let mut new_req = S3Request::new(head_object_input);

                        new_req.headers.insert(
                            "X-SKYSTORE-WARMUP",
                            warmup_req.warmup_regions.join(",").parse().unwrap(),
                        );

                        proxy_clone
                            .head_object(new_req)
                            .await
                            .map_err(S3Error::internal_error)?;

                        Ok::<_, S3Error>(hyper::Response::new(s3s::Body::from(Vec::new())))
                    } else {
                        let res = hyper::Response::builder()
                            .status(hyper::StatusCode::BAD_REQUEST)
                            .body(s3s::Body::from("Bad request".to_string()))
                            .expect("Failed to construct the response");

                        Ok::<_, S3Error>(res)
                    }
                };
                Box::pin(fut)
            } else {
                s3_service.call(req).boxed()
            }
        });

    // Run server
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 8002));
    info!("Starting server on {}", addr);
    hyper::Server::bind(&addr)
        .serve(tower::make::Shared::new(service))
        .await
        .expect("server error");
}
