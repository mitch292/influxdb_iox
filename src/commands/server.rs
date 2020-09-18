#![deny(rust_2018_idioms)]

use tracing::{debug, info};

use delorean_generated_types::{delorean_server::DeloreanServer, storage_server::StorageServer};
use delorean_partitioned_store::storage::database::Database;

use std::env::VarError;
use std::net::SocketAddr;
use std::sync::Arc;

use hyper::service::{make_service_fn, service_fn};
use hyper::Server;

use crate::server::http_routes;
use crate::server::rpc::GrpcServer;
use crate::server::App;

/// Main entrypoint of the Delorean server loop
pub async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    dotenv::dotenv().ok();

    let db_dir = match std::env::var("DELOREAN_DB_DIR") {
        Ok(val) => val,
        Err(_) => {
            // default database path is $HOME/.delorean
            let mut path = dirs::home_dir().unwrap();
            path.push(".delorean/");
            path.into_os_string().into_string().unwrap()
        }
    };
    debug!("Delorean Server using database directory: {:?}", db_dir);

    let db = Database::new(&db_dir);
    db.restore_from_wal().await?;

    let state = Arc::new(App { db });
    let bind_addr: SocketAddr = match std::env::var("DELOREAN_BIND_ADDR") {
        Ok(addr) => addr
            .parse()
            .expect("DELOREAN_BIND_ADDR environment variable not a valid SocketAddr"),
        Err(VarError::NotPresent) => "127.0.0.1:8080".parse().unwrap(),
        Err(VarError::NotUnicode(_)) => {
            panic!("DELOREAN_BIND_ADDR environment variable not a valid unicode string")
        }
    };
    let grpc_bind_addr: SocketAddr = match std::env::var("DELOREAN_GRPC_BIND_ADDR") {
        Ok(addr) => addr
            .parse()
            .expect("DELOREAN_GRPC_BIND_ADDR environment variable not a valid SocketAddr"),
        Err(VarError::NotPresent) => "127.0.0.1:8082".parse().unwrap(),
        Err(VarError::NotUnicode(_)) => {
            panic!("DELOREAN_GRPC_BIND_ADDR environment variable not a valid unicode string")
        }
    };

    let grpc_server = tonic::transport::Server::builder()
        .add_service(DeloreanServer::new(GrpcServer { app: state.clone() }))
        .add_service(StorageServer::new(GrpcServer { app: state.clone() }))
        .serve(grpc_bind_addr);

    let make_svc = make_service_fn(move |_conn| {
        let state = state.clone();
        async move {
            Ok::<_, http::Error>(service_fn(move |req| {
                let state = state.clone();
                http_routes::service(req, state)
            }))
        }
    });

    let server = Server::bind(&bind_addr).serve(make_svc);

    info!("gRPC server listening on http://{}", grpc_bind_addr);
    info!("Listening on http://{}", bind_addr);

    let (grpc_server, server) = futures::future::join(grpc_server, server).await;
    grpc_server?;
    server?;

    Ok(())
}
