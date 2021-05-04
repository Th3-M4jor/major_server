use tokio::sync::mpsc;
use warp::{Filter, http::{HeaderValue, Response, header::CONTENT_TYPE}, hyper::StatusCode};

#[cfg(debug_assertions)]
use tracing::Level;
#[cfg(debug_assertions)]
use tracing_subscriber::FmtSubscriber;

mod socket_manager;

use socket_manager::*;

#[tokio::main]
async fn main() {

    #[cfg(debug_assertions)]
    {

        let subscriber = FmtSubscriber::builder()
            // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
            // will be written to stdout.
            .with_max_level(Level::TRACE)
            // completes the builder.
            .finish();

            tracing::subscriber::set_global_default(subscriber)
            .expect("setting default subscriber failed");
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();

    ctrlc::set_handler(move || {
        tx.send(()).unwrap();
    }).expect("err setting SIGINT handler");

    let all_else = warp::any().map(||{
        let response = Response::builder();
        response.status(404).body("bad endpoint")
    });

    let groups = Groups::default();
    groups.spawn_collector_loop();
    let folder_groups = warp::any().map(move || groups.clone());
    let folder_groups_2 = folder_groups.clone();

    let get_groups = warp::get().and(
        warp::path!("manager" / "api" / "groups").and(folder_groups_2).map(|groups: Groups| -> Box<dyn warp::Reply> {
            match groups.get_group_list() {
                Some(list) => {
                    let mut resp = Response::new(list);
                    resp.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                    Box::new(resp)
                }
                None => {
                    Box::new(StatusCode::NO_CONTENT)
                }
            }
        })
    );

    let socket = warp::path!("manager" / "api" / "join" / GroupName / PlayerName)
        .and(warp::ws())
        .and(folder_groups)
        .map(|group_name: String, player_name: String, ws: warp::ws::Ws, groups|{
        
            #[cfg(debug_assertions)]
            println!("Recieved join request for {}/{}", group_name, player_name);
            
            ws.on_upgrade(move |socket| user_connected(socket, groups, player_name, group_name))
        });

    let paths = socket.or(get_groups).or(all_else);

    let (_, server) = warp::serve(paths).bind_with_graceful_shutdown(([0,0,0,0], 8080), async move {
        let _ = rx.recv().await;
    });

    server.await;

}