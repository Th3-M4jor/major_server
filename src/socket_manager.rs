use warp::ws::{WebSocket, Message};
use tokio::sync::mpsc;//, RwLock};
use parking_lot::RwLock;
use futures::{FutureExt, StreamExt};
use tokio_stream::wrappers::UnboundedReceiverStream;
//use once_cell::sync::Lazy;
use std::collections::HashMap;
use dashmap::DashMap;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::borrow::Cow;
use tokio::time::Instant;
//use mysql_async::prelude::*;
use serde::{Serialize, Deserialize};
use percent_encoding::percent_decode_str;

pub(crate) type GroupName = String;
pub(crate) type PlayerName = String;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct FolderChip {
    name: String,
    used: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]                       //only one field so, "flatten" before serializing
pub(crate) struct Folder {
    chips: Vec<FolderChip>,
    #[serde(skip)]
    socket: mpsc::UnboundedSender<Result<Message, warp::Error>>,
}

#[derive(Serialize, Clone)]
pub(crate) enum SocketMsg<'a> {
    FoldersUpdated(Cow<'a,HashMap<PlayerName, Folder>>),
    Error(Cow<'a, str>),
    Ready,
}

#[derive(Debug)]
pub(crate) struct PlayerGroup {
    pub data: RwLock<HashMap<PlayerName, Folder>>,
    pub creation_time: Instant,
}

impl PlayerGroup {
    /// Sends a msg to all players
    pub(crate) fn send_group_msg(&self, msg: &[u8]) {
        let guard = self.data.read_recursive();
        for fldr in guard.values() {
            let _ = fldr.socket.send(Ok(Message::binary(msg)));
        }
    }
}

impl Default for PlayerGroup {
    fn default() -> Self {
        Self {
            data: RwLock::new(HashMap::new()),
            creation_time: Instant::now(),
        }
    }
}

#[derive(Clone, Default, Debug)]
pub(crate) struct Groups {
    pub data: Arc<DashMap<GroupName, Arc<PlayerGroup>>>,
    cleanup_loop_running: Arc<AtomicBool>,
}

impl Groups {
    pub(crate) fn spawn_collector_loop(&self) {
        let res = self.cleanup_loop_running.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire);
        if res.is_err() {
            return; // already running
        }
        let self_clone = self.clone();
        tokio::spawn(async move {
            self_clone.collector_loop().await;
            self_clone.cleanup_loop_running.store(false, Ordering::Release);
        });
    }

    async fn collector_loop(&self) {
        let duration = std::time::Duration::from_secs(3600 * 2); //every two hours
        let start = Instant::now() + duration;
        let mut interval = tokio::time::interval_at(start, duration);

        let closed_msg = bincode::serialize(&SocketMsg::Error(Cow::Borrowed("Group open too long, closed by server"))).unwrap();

        // should run indefinitely
        loop {
            interval.tick().await;

            if self.data.len() == 0 {
                continue;
                // do nothing, groups are empty
            }

            let mut groups_to_close: Vec<(GroupName, Arc<PlayerGroup>)> = Vec::new();
            self.data.retain(|name, grp| {
                // (3600 * 12) = 12 hours
                if grp.creation_time.elapsed().as_secs() >= (3600u64 * 12u64) {
                    groups_to_close.push((name.to_owned(), Arc::clone(grp)));
                    false
                } else {
                    true
                }
            });

            if groups_to_close.is_empty() {
                continue; // No groups to close
            }

            for group in groups_to_close {
                group.1.send_group_msg(&closed_msg);
            }

        }
    }

    pub(crate) fn add_user_to_group(&self, player_name: &str, group_name: &str, socket: &mpsc::UnboundedSender<Result<Message, warp::Error>>) -> Result<(), Vec<u8>> {
        
        if group_name.trim().is_empty() || player_name.trim().is_empty() {
            let msg = Cow::Borrowed("Cannot have an empty name or group");
            return Err(bincode::serialize(&SocketMsg::Error(msg)).unwrap());
        }
        
        let group = self.data.entry(group_name.to_owned()).or_insert_with(|| Arc::new(PlayerGroup::default()));
        
        //let players_lock = group.value().data;
        let mut players = group.value().data.write();
        if players.contains_key(player_name) {
            let msg = Cow::Owned(format!("The name {} is already taken for this group", player_name));
            return Err(bincode::serialize(&SocketMsg::Error(msg)).unwrap());
        }

        let _ = players.insert(player_name.to_owned(), Folder{chips: vec![], socket: socket.clone()});
    
        Ok(())
    }

    pub(crate) async fn update_folder(&self, player_name: &str, group_name: &str, chips: Vec<FolderChip>) {
        let group_guard = match self.data.get(group_name) {
            Some(group) => group,
            None => return,
        };
        let group_lock = &group_guard.value().data;
        let mut group = group_lock.write();
        let folder = match group.get_mut(player_name) {
            Some(folder) => folder,
            None => return,
        };
        folder.chips = chips;
    }

    pub(crate) async fn folders_updated(&self, player_name: &str, group_name: &str) {
        let group_guard = match self.data.get(group_name) {
            Some(group) => group,
            None => return,
        };
        let group_lock = Arc::clone(&group_guard.value());
        let player_name = player_name.to_owned();
        tokio::task::spawn_blocking(move || {
            let group = group_lock.data.read();
            let msg = 
            match bincode::serialize(&SocketMsg::FoldersUpdated(Cow::Borrowed(&*group))) {
                Ok(val) => val,
                Err(why) => bincode::serialize(&SocketMsg::Error(Cow::Owned(why.to_string()))).unwrap(),
            };

            for(name, folder) in group.iter() {
                if player_name == name.as_str() {
                    continue;
                }
                let _ = folder.socket.send(Ok(Message::binary(msg.as_slice())));
            }

        }).await.expect("Spawn blocking panicked, shouldn't have happened");
        
    }

    pub(crate) fn send_to_new_player(&self, player_name: &str, group_name: &str) {
        let group_guard = match self.data.get(group_name) {
            Some(group) => group,
            None => return,
        };
        let group_lock = &group_guard.value().data;
        let group = group_lock.read_recursive();
        let msg = 
            match bincode::serialize(&SocketMsg::FoldersUpdated(Cow::Borrowed(&*group))) {
                Ok(val) => val,
                Err(why) => bincode::serialize(&SocketMsg::Error(Cow::Owned(why.to_string()))).unwrap(),
            };
        let _ = group.get(player_name).unwrap().socket.send(Ok(Message::binary(msg)));
    }

    pub(crate) fn left_group(&self, player_name: &str, group_name: &str) {
        let group_guard = match self.data.get(group_name) {
            Some(group) => group,
            None => return,
        };
        let group_lock = &group_guard.value().data;
        let mut group = group_lock.write();
        group.remove(player_name);
        
        if group.len() != 0 {
            return;
        }

        //drop all locks/guards before removing
        drop(group);
        drop(group_lock);
        drop(group_guard);

        self.data.remove(group_name);
    }

    pub(crate) fn get_group_list(&self) -> Option<String> {
        let count = self.data.len();
        if count == 0 {
            return None;
        }
        let mut to_ret = Vec::with_capacity(count);
        for val in self.data.iter() {
            to_ret.push(val.key().to_owned());
        }

        serde_json::to_string(&to_ret).ok()
    }    

}

async fn user_connected_inner(ws: WebSocket, groups: Groups, player_name: PlayerName, group_name: GroupName) {


    let (user_ws_tx, mut user_ws_rx) = ws.split();
    let (tx, rx) = mpsc::unbounded_channel();
    let rx_stream = UnboundedReceiverStream::new(rx);
    tokio::task::spawn(rx_stream.forward(user_ws_tx).map(|result| {
        if let Err(e) = result {
            println!("ws error: {}", e);
        }
    }));

    // unable to add user to group for some reason
    if let Err(why) = groups.add_user_to_group(&player_name, &group_name, &tx) {
        let _ = tx.send(Ok(Message::binary(why)));
        return;
    }

    let msg = bincode::serialize(&SocketMsg::Ready).unwrap();
    if let Err(_) = tx.send(Ok(Message::binary(msg))) {
        return;
    }

    groups.send_to_new_player(&player_name, &group_name);

    while let Some(result) = user_ws_rx.next().await {
        let msg = match result{
            Ok(msg) => msg,
            Err(_why) => {
                #[cfg(debug_assertions)]
                {
                    eprintln!("Socket error occurred: {}", _why.to_string());
                }
                break;
            }
        };

        let folder_res = bincode::deserialize::<Vec<FolderChip>>(msg.as_bytes());

        match folder_res {
            Ok(folder) => {

                #[cfg(debug_assertions)]
                println!("{:?}", folder);

                groups.update_folder(&player_name, &group_name, folder).await;
                groups.folders_updated(&player_name, &group_name).await;
            },
            Err(_err) => {
                #[cfg(debug_assertions)]
                {
                    eprintln!("Folder deserialization failed: {:?}", _err);
                }
                
                break;
                //let _ = db_manager::socket_log(&ip, &msg).await;
            }
        }
    }

    groups.left_group(&player_name, &group_name);
    groups.folders_updated(&player_name, &group_name).await;
}

pub (crate) async fn user_connected(ws: WebSocket, groups: Groups, player_name: PlayerName, group_name: GroupName) {
    
    #[cfg(debug_assertions)]
    println!("Connection upgraded for {}/{}", group_name, player_name);
    
    let player_name_res = percent_decode_str(&player_name).decode_utf8();
    let group_name_res = percent_decode_str(&group_name).decode_utf8();
    let (player_name, group_name) = match (player_name_res, group_name_res) {
        (Ok(player), Ok(group)) => (player.trim().to_string(), group.trim().to_string()),
            _ => return,
    };

    if !player_name.is_ascii() || !group_name.is_ascii() {
        return; //refuse names which aren't ascii
    }

    tokio::task::spawn(user_connected_inner(ws, groups, player_name, group_name));
}