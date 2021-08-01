pub mod abstraction;

pub mod account_data;
pub mod admin;
pub mod appservice;
pub mod globals;
pub mod key_backups;
pub mod media;
pub mod proxy;
pub mod pusher;
pub mod rooms;
pub mod sending;
pub mod transaction_ids;
pub mod uiaa;
pub mod users;

use crate::{utils, Error, Result};
use abstraction::DatabaseEngine;
use directories::ProjectDirs;
use lru_cache::LruCache;
use rocket::{
    futures::{channel::mpsc, stream::FuturesUnordered, StreamExt},
    outcome::{try_outcome, IntoOutcome},
    request::{FromRequest, Request},
    Shutdown, State,
};
use ruma::{DeviceId, EventId, RoomId, ServerName, UserId};
use serde::{de::IgnoredAny, Deserialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    convert::TryFrom,
    fs::{self, remove_dir_all},
    io::Write,
    mem::size_of,
    ops::Deref,
    path::Path,
    sync::{Arc, Mutex, RwLock},
};
use tokio::sync::{OwnedRwLockReadGuard, RwLock as TokioRwLock, Semaphore};
use tracing::{debug, error, warn};

use self::proxy::ProxyConfig;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    server_name: Box<ServerName>,
    database_path: String,
    #[serde(default = "default_db_cache_capacity_mb")]
    db_cache_capacity_mb: f64,
    #[serde(default = "default_sqlite_read_pool_size")]
    sqlite_read_pool_size: usize,
    #[serde(default = "true_fn")]
    sqlite_wal_clean_timer: bool,
    #[serde(default = "default_sqlite_wal_clean_second_interval")]
    sqlite_wal_clean_second_interval: u32,
    #[serde(default = "default_sqlite_wal_clean_second_timeout")]
    sqlite_wal_clean_second_timeout: u32,
    #[serde(default = "default_sqlite_spillover_reap_fraction")]
    sqlite_spillover_reap_fraction: f64,
    #[serde(default = "default_sqlite_spillover_reap_interval_secs")]
    sqlite_spillover_reap_interval_secs: u32,
    #[serde(default = "default_max_request_size")]
    max_request_size: u32,
    #[serde(default = "default_max_concurrent_requests")]
    max_concurrent_requests: u16,
    #[serde(default = "true_fn")]
    allow_registration: bool,
    #[serde(default = "true_fn")]
    allow_encryption: bool,
    #[serde(default = "false_fn")]
    allow_federation: bool,
    #[serde(default = "false_fn")]
    pub allow_jaeger: bool,
    #[serde(default = "false_fn")]
    pub tracing_flame: bool,
    #[serde(default)]
    proxy: ProxyConfig,
    jwt_secret: Option<String>,
    #[serde(default = "Vec::new")]
    trusted_servers: Vec<Box<ServerName>>,
    #[serde(default = "default_log")]
    pub log: String,

    #[serde(flatten)]
    catchall: BTreeMap<String, IgnoredAny>,
}

const DEPRECATED_KEYS: &[&str] = &["cache_capacity"];

impl Config {
    pub fn warn_deprecated(&self) {
        let mut was_deprecated = false;
        for key in self
            .catchall
            .keys()
            .filter(|key| DEPRECATED_KEYS.iter().any(|s| s == key))
        {
            warn!("Config parameter {} is deprecated", key);
            was_deprecated = true;
        }

        if was_deprecated {
            warn!("Read conduit documentation and check your configuration if any new configuration parameters should be adjusted");
        }
    }
}

fn false_fn() -> bool {
    false
}

fn true_fn() -> bool {
    true
}

fn default_db_cache_capacity_mb() -> f64 {
    200.0
}

fn default_sqlite_read_pool_size() -> usize {
    num_cpus::get().max(1)
}

fn default_sqlite_wal_clean_second_interval() -> u32 {
    60 * 60
}

fn default_sqlite_wal_clean_second_timeout() -> u32 {
    2
}

fn default_sqlite_spillover_reap_fraction() -> f64 {
    0.5
}

fn default_sqlite_spillover_reap_interval_secs() -> u32 {
    60
}

fn default_max_request_size() -> u32 {
    20 * 1024 * 1024 // Default to 20 MB
}

fn default_max_concurrent_requests() -> u16 {
    100
}

fn default_log() -> String {
    "info,state_res=warn,rocket=off,_=off,sled=off".to_owned()
}

#[cfg(feature = "sled")]
pub type Engine = abstraction::sled::Engine;

#[cfg(feature = "rocksdb")]
pub type Engine = abstraction::rocksdb::Engine;

#[cfg(feature = "sqlite")]
pub type Engine = abstraction::sqlite::Engine;

#[cfg(feature = "heed")]
pub type Engine = abstraction::heed::Engine;

pub struct Database {
    _db: Arc<Engine>,
    pub globals: globals::Globals,
    pub users: users::Users,
    pub uiaa: uiaa::Uiaa,
    pub rooms: rooms::Rooms,
    pub account_data: account_data::AccountData,
    pub media: media::Media,
    pub key_backups: key_backups::KeyBackups,
    pub transaction_ids: transaction_ids::TransactionIds,
    pub sending: sending::Sending,
    pub admin: admin::Admin,
    pub appservice: appservice::Appservice,
    pub pusher: pusher::PushData,
}

impl Database {
    /// Tries to remove the old database but ignores all errors.
    pub fn try_remove(server_name: &str) -> Result<()> {
        let mut path = ProjectDirs::from("xyz", "koesters", "conduit")
            .ok_or_else(|| Error::bad_config("The OS didn't return a valid home directory path."))?
            .data_dir()
            .to_path_buf();
        path.push(server_name);
        let _ = remove_dir_all(path);

        Ok(())
    }

    fn check_sled_or_sqlite_db(config: &Config) -> Result<()> {
        #[cfg(feature = "backend_sqlite")]
        {
            let path = Path::new(&config.database_path);

            let sled_exists = path.join("db").exists();
            let sqlite_exists = path.join("conduit.db").exists();
            if sled_exists {
                if sqlite_exists {
                    // most likely an in-place directory, only warn
                    warn!("Both sled and sqlite databases are detected in database directory");
                    warn!("Currently running from the sqlite database, but consider removing sled database files to free up space")
                } else {
                    error!(
                        "Sled database detected, conduit now uses sqlite for database operations"
                    );
                    error!("This database must be converted to sqlite, go to https://github.com/ShadowJonathan/conduit_toolbox#conduit_sled_to_sqlite");
                    return Err(Error::bad_config(
                        "sled database detected, migrate to sqlite",
                    ));
                }
            }
        }

        Ok(())
    }

    /// Load an existing database or create a new one.
    pub async fn load_or_create(config: &Config) -> Result<Arc<TokioRwLock<Self>>> {
        Self::check_sled_or_sqlite_db(&config)?;

        let builder = Engine::open(&config)?;

        if config.max_request_size < 1024 {
            eprintln!("ERROR: Max request size is less than 1KB. Please increase it.");
        }

        let (admin_sender, admin_receiver) = mpsc::unbounded();
        let (sending_sender, sending_receiver) = mpsc::unbounded();

        let db = Arc::new(TokioRwLock::from(Self {
            _db: builder.clone(),
            users: users::Users {
                userid_password: builder.open_tree("userid_password")?,
                userid_displayname: builder.open_tree("userid_displayname")?,
                userid_avatarurl: builder.open_tree("userid_avatarurl")?,
                userid_blurhash: builder.open_tree("userid_blurhash")?,
                userdeviceid_token: builder.open_tree("userdeviceid_token")?,
                userdeviceid_metadata: builder.open_tree("userdeviceid_metadata")?,
                userid_devicelistversion: builder.open_tree("userid_devicelistversion")?,
                token_userdeviceid: builder.open_tree("token_userdeviceid")?,
                onetimekeyid_onetimekeys: builder.open_tree("onetimekeyid_onetimekeys")?,
                userid_lastonetimekeyupdate: builder.open_tree("userid_lastonetimekeyupdate")?,
                keychangeid_userid: builder.open_tree("keychangeid_userid")?,
                keyid_key: builder.open_tree("keyid_key")?,
                userid_masterkeyid: builder.open_tree("userid_masterkeyid")?,
                userid_selfsigningkeyid: builder.open_tree("userid_selfsigningkeyid")?,
                userid_usersigningkeyid: builder.open_tree("userid_usersigningkeyid")?,
                todeviceid_events: builder.open_tree("todeviceid_events")?,
            },
            uiaa: uiaa::Uiaa {
                userdevicesessionid_uiaainfo: builder.open_tree("userdevicesessionid_uiaainfo")?,
                userdevicesessionid_uiaarequest: builder
                    .open_tree("userdevicesessionid_uiaarequest")?,
            },
            rooms: rooms::Rooms {
                edus: rooms::RoomEdus {
                    readreceiptid_readreceipt: builder.open_tree("readreceiptid_readreceipt")?,
                    roomuserid_privateread: builder.open_tree("roomuserid_privateread")?, // "Private" read receipt
                    roomuserid_lastprivatereadupdate: builder
                        .open_tree("roomuserid_lastprivatereadupdate")?,
                    typingid_userid: builder.open_tree("typingid_userid")?,
                    roomid_lasttypingupdate: builder.open_tree("roomid_lasttypingupdate")?,
                    presenceid_presence: builder.open_tree("presenceid_presence")?,
                    userid_lastpresenceupdate: builder.open_tree("userid_lastpresenceupdate")?,
                },
                pduid_pdu: builder.open_tree("pduid_pdu")?,
                eventid_pduid: builder.open_tree("eventid_pduid")?,
                roomid_pduleaves: builder.open_tree("roomid_pduleaves")?,

                alias_roomid: builder.open_tree("alias_roomid")?,
                aliasid_alias: builder.open_tree("aliasid_alias")?,
                publicroomids: builder.open_tree("publicroomids")?,

                tokenids: builder.open_tree("tokenids")?,

                roomserverids: builder.open_tree("roomserverids")?,
                serverroomids: builder.open_tree("serverroomids")?,
                userroomid_joined: builder.open_tree("userroomid_joined")?,
                roomuserid_joined: builder.open_tree("roomuserid_joined")?,
                roomuseroncejoinedids: builder.open_tree("roomuseroncejoinedids")?,
                userroomid_invitestate: builder.open_tree("userroomid_invitestate")?,
                roomuserid_invitecount: builder.open_tree("roomuserid_invitecount")?,
                userroomid_leftstate: builder.open_tree("userroomid_leftstate")?,
                roomuserid_leftcount: builder.open_tree("roomuserid_leftcount")?,

                userroomid_notificationcount: builder.open_tree("userroomid_notificationcount")?,
                userroomid_highlightcount: builder.open_tree("userroomid_highlightcount")?,

                statekey_shortstatekey: builder.open_tree("statekey_shortstatekey")?,

                shortroomid_roomid: builder.open_tree("shortroomid_roomid")?,
                roomid_shortroomid: builder.open_tree("roomid_shortroomid")?,

                stateid_shorteventid: builder.open_tree("stateid_shorteventid")?,
                shortstatehash_statediff: builder.open_tree("shortstatehash_statediff")?,
                eventid_shorteventid: builder.open_tree("eventid_shorteventid")?,
                shorteventid_eventid: builder.open_tree("shorteventid_eventid")?,
                shorteventid_shortstatehash: builder.open_tree("shorteventid_shortstatehash")?,
                roomid_shortstatehash: builder.open_tree("roomid_shortstatehash")?,
                statehash_shortstatehash: builder.open_tree("statehash_shortstatehash")?,

                eventid_outlierpdu: builder.open_tree("eventid_outlierpdu")?,
                referencedevents: builder.open_tree("referencedevents")?,
                pdu_cache: Mutex::new(LruCache::new(100_000)),
                auth_chain_cache: Mutex::new(LruCache::new(100_000)),
            },
            account_data: account_data::AccountData {
                roomuserdataid_accountdata: builder.open_tree("roomuserdataid_accountdata")?,
                roomusertype_roomuserdataid: builder.open_tree("roomusertype_roomuserdataid")?,
            },
            media: media::Media {
                mediaid_file: builder.open_tree("mediaid_file")?,
            },
            key_backups: key_backups::KeyBackups {
                backupid_algorithm: builder.open_tree("backupid_algorithm")?,
                backupid_etag: builder.open_tree("backupid_etag")?,
                backupkeyid_backup: builder.open_tree("backupkeyid_backup")?,
            },
            transaction_ids: transaction_ids::TransactionIds {
                userdevicetxnid_response: builder.open_tree("userdevicetxnid_response")?,
            },
            sending: sending::Sending {
                servername_educount: builder.open_tree("servername_educount")?,
                servernameevent_data: builder.open_tree("servernameevent_data")?,
                servercurrentevent_data: builder.open_tree("servercurrentevent_data")?,
                maximum_requests: Arc::new(Semaphore::new(config.max_concurrent_requests as usize)),
                sender: sending_sender,
            },
            admin: admin::Admin {
                sender: admin_sender,
            },
            appservice: appservice::Appservice {
                cached_registrations: Arc::new(RwLock::new(HashMap::new())),
                id_appserviceregistrations: builder.open_tree("id_appserviceregistrations")?,
            },
            pusher: pusher::PushData {
                senderkey_pusher: builder.open_tree("senderkey_pusher")?,
            },
            globals: globals::Globals::load(
                builder.open_tree("global")?,
                builder.open_tree("server_signingkeys")?,
                config.clone(),
            )?,
        }));

        {
            let db = db.read().await;
            // MIGRATIONS
            // TODO: database versions of new dbs should probably not be 0
            if db.globals.database_version()? < 1 {
                for (roomserverid, _) in db.rooms.roomserverids.iter() {
                    let mut parts = roomserverid.split(|&b| b == 0xff);
                    let room_id = parts.next().expect("split always returns one element");
                    let servername = match parts.next() {
                        Some(s) => s,
                        None => {
                            error!("Migration: Invalid roomserverid in db.");
                            continue;
                        }
                    };
                    let mut serverroomid = servername.to_vec();
                    serverroomid.push(0xff);
                    serverroomid.extend_from_slice(room_id);

                    db.rooms.serverroomids.insert(&serverroomid, &[])?;
                }

                db.globals.bump_database_version(1)?;

                println!("Migration: 0 -> 1 finished");
            }

            if db.globals.database_version()? < 2 {
                // We accidentally inserted hashed versions of "" into the db instead of just ""
                for (userid, password) in db.users.userid_password.iter() {
                    let password = utils::string_from_bytes(&password);

                    let empty_hashed_password = password.map_or(false, |password| {
                        argon2::verify_encoded(&password, b"").unwrap_or(false)
                    });

                    if empty_hashed_password {
                        db.users.userid_password.insert(&userid, b"")?;
                    }
                }

                db.globals.bump_database_version(2)?;

                println!("Migration: 1 -> 2 finished");
            }

            if db.globals.database_version()? < 3 {
                // Move media to filesystem
                for (key, content) in db.media.mediaid_file.iter() {
                    if content.is_empty() {
                        continue;
                    }

                    let path = db.globals.get_media_file(&key);
                    let mut file = fs::File::create(path)?;
                    file.write_all(&content)?;
                    db.media.mediaid_file.insert(&key, &[])?;
                }

                db.globals.bump_database_version(3)?;

                println!("Migration: 2 -> 3 finished");
            }

            if db.globals.database_version()? < 4 {
                // Add federated users to db as deactivated
                for our_user in db.users.iter() {
                    let our_user = our_user?;
                    if db.users.is_deactivated(&our_user)? {
                        continue;
                    }
                    for room in db.rooms.rooms_joined(&our_user) {
                        for user in db.rooms.room_members(&room?) {
                            let user = user?;
                            if user.server_name() != db.globals.server_name() {
                                println!("Migration: Creating user {}", user);
                                db.users.create(&user, None)?;
                            }
                        }
                    }
                }

                db.globals.bump_database_version(4)?;

                println!("Migration: 3 -> 4 finished");
            }

            if db.globals.database_version()? < 5 {
                // Upgrade user data store
                for (roomuserdataid, _) in db.account_data.roomuserdataid_accountdata.iter() {
                    let mut parts = roomuserdataid.split(|&b| b == 0xff);
                    let room_id = parts.next().unwrap();
                    let user_id = parts.next().unwrap();
                    let event_type = roomuserdataid.rsplit(|&b| b == 0xff).next().unwrap();

                    let mut key = room_id.to_vec();
                    key.push(0xff);
                    key.extend_from_slice(user_id);
                    key.push(0xff);
                    key.extend_from_slice(event_type);

                    db.account_data
                        .roomusertype_roomuserdataid
                        .insert(&key, &roomuserdataid)?;
                }

                db.globals.bump_database_version(5)?;

                println!("Migration: 4 -> 5 finished");
            }

            fn load_shortstatehash_info(
                shortstatehash: &[u8],
                db: &Database,
                lru: &mut LruCache<
                    Vec<u8>,
                    Vec<(
                        Vec<u8>,
                        HashSet<Vec<u8>>,
                        HashSet<Vec<u8>>,
                        HashSet<Vec<u8>>,
                    )>,
                >,
            ) -> Result<
                Vec<(
                    Vec<u8>,          // sstatehash
                    HashSet<Vec<u8>>, // full state
                    HashSet<Vec<u8>>, // added
                    HashSet<Vec<u8>>, // removed
                )>,
            > {
                if let Some(result) = lru.get_mut(shortstatehash) {
                    return Ok(result.clone());
                }

                let value = db
                    .rooms
                    .shortstatehash_statediff
                    .get(shortstatehash)?
                    .ok_or_else(|| Error::bad_database("State hash does not exist"))?;
                let parent = value[0..size_of::<u64>()].to_vec();

                let mut add_mode = true;
                let mut added = HashSet::new();
                let mut removed = HashSet::new();

                let mut i = size_of::<u64>();
                while let Some(v) = value.get(i..i + 2 * size_of::<u64>()) {
                    if add_mode && v.starts_with(&0_u64.to_be_bytes()) {
                        add_mode = false;
                        i += size_of::<u64>();
                        continue;
                    }
                    if add_mode {
                        added.insert(v.to_vec());
                    } else {
                        removed.insert(v.to_vec());
                    }
                    i += 2 * size_of::<u64>();
                }

                if parent != 0_u64.to_be_bytes() {
                    let mut response = load_shortstatehash_info(&parent, db, lru)?;
                    let mut state = response.last().unwrap().1.clone();
                    state.extend(added.iter().cloned());
                    for r in &removed {
                        state.remove(r);
                    }

                    response.push((shortstatehash.to_vec(), state, added, removed));

                    lru.insert(shortstatehash.to_vec(), response.clone());
                    Ok(response)
                } else {
                    let mut response = Vec::new();
                    response.push((shortstatehash.to_vec(), added.clone(), added, removed));
                    lru.insert(shortstatehash.to_vec(), response.clone());
                    Ok(response)
                }
            }

            fn update_shortstatehash_level(
                current_shortstatehash: &[u8],
                statediffnew: HashSet<Vec<u8>>,
                statediffremoved: HashSet<Vec<u8>>,
                diff_to_sibling: usize,
                mut parent_states: Vec<(
                    Vec<u8>,          // sstatehash
                    HashSet<Vec<u8>>, // full state
                    HashSet<Vec<u8>>, // added
                    HashSet<Vec<u8>>, // removed
                )>,
                db: &Database,
            ) -> Result<()> {
                let diffsum = statediffnew.len() + statediffremoved.len();

                if parent_states.len() > 3 {
                    // Number of layers
                    // To many layers, we have to go deeper
                    let parent = parent_states.pop().unwrap();

                    let mut parent_new = parent.2;
                    let mut parent_removed = parent.3;

                    for removed in statediffremoved {
                        if !parent_new.remove(&removed) {
                            parent_removed.insert(removed);
                        }
                    }
                    parent_new.extend(statediffnew);

                    update_shortstatehash_level(
                        current_shortstatehash,
                        parent_new,
                        parent_removed,
                        diffsum,
                        parent_states,
                        db,
                    )?;

                    return Ok(());
                }

                if parent_states.len() == 0 {
                    // There is no parent layer, create a new state
                    let mut value = 0_u64.to_be_bytes().to_vec(); // 0 means no parent
                    for new in &statediffnew {
                        value.extend_from_slice(&new);
                    }

                    if !statediffremoved.is_empty() {
                        warn!("Tried to create new state with removals");
                    }

                    db.rooms
                        .shortstatehash_statediff
                        .insert(&current_shortstatehash, &value)?;

                    return Ok(());
                };

                // Else we have two options.
                // 1. We add the current diff on top of the parent layer.
                // 2. We replace a layer above

                let parent = parent_states.pop().unwrap();
                let parent_diff = parent.2.len() + parent.3.len();

                if diffsum * diffsum >= 2 * diff_to_sibling * parent_diff {
                    // Diff too big, we replace above layer(s)
                    let mut parent_new = parent.2;
                    let mut parent_removed = parent.3;

                    for removed in statediffremoved {
                        if !parent_new.remove(&removed) {
                            parent_removed.insert(removed);
                        }
                    }

                    parent_new.extend(statediffnew);
                    update_shortstatehash_level(
                        current_shortstatehash,
                        parent_new,
                        parent_removed,
                        diffsum,
                        parent_states,
                        db,
                    )?;
                } else {
                    // Diff small enough, we add diff as layer on top of parent
                    let mut value = parent.0.clone();
                    for new in &statediffnew {
                        value.extend_from_slice(&new);
                    }

                    if !statediffremoved.is_empty() {
                        value.extend_from_slice(&0_u64.to_be_bytes());
                        for removed in &statediffremoved {
                            value.extend_from_slice(&removed);
                        }
                    }

                    db.rooms
                        .shortstatehash_statediff
                        .insert(&current_shortstatehash, &value)?;
                }

                Ok(())
            }

            if db.globals.database_version()? < 6 {
                // Upgrade state store
                let mut lru = LruCache::new(1000);
                let mut last_roomstates: HashMap<RoomId, Vec<u8>> = HashMap::new();
                let mut current_sstatehash: Vec<u8> = Vec::new();
                let mut current_room = None;
                let mut current_state = HashSet::new();
                let mut counter = 0;
                for (k, seventid) in db._db.open_tree("stateid_shorteventid")?.iter() {
                    let sstatehash = k[0..size_of::<u64>()].to_vec();
                    let sstatekey = k[size_of::<u64>()..].to_vec();
                    if sstatehash != current_sstatehash {
                        if !current_sstatehash.is_empty() {
                            counter += 1;
                            println!("counter: {}", counter);
                            let current_room = current_room.as_ref().unwrap();
                            let last_roomsstatehash = last_roomstates.get(&current_room);

                            let states_parents = last_roomsstatehash.map_or_else(
                                || Ok(Vec::new()),
                                |last_roomsstatehash| {
                                    load_shortstatehash_info(&last_roomsstatehash, &db, &mut lru)
                                },
                            )?;

                            let (statediffnew, statediffremoved) =
                                if let Some(parent_stateinfo) = states_parents.last() {
                                    let statediffnew = current_state
                                        .difference(&parent_stateinfo.1)
                                        .cloned()
                                        .collect::<HashSet<_>>();

                                    let statediffremoved = parent_stateinfo
                                        .1
                                        .difference(&current_state)
                                        .cloned()
                                        .collect::<HashSet<_>>();

                                    (statediffnew, statediffremoved)
                                } else {
                                    (current_state, HashSet::new())
                                };

                            update_shortstatehash_level(
                                &current_sstatehash,
                                statediffnew,
                                statediffremoved,
                                2, // every state change is 2 event changes on average
                                states_parents,
                                &db,
                            )?;

                            /*
                            let mut tmp = load_shortstatehash_info(&current_sstatehash, &db)?;
                            let state = tmp.pop().unwrap();
                            println!(
                                "{}\t{}{:?}: {:?} + {:?} - {:?}",
                                current_room,
                                "  ".repeat(tmp.len()),
                                utils::u64_from_bytes(&current_sstatehash).unwrap(),
                                tmp.last().map(|b| utils::u64_from_bytes(&b.0).unwrap()),
                                state
                                    .2
                                    .iter()
                                    .map(|b| utils::u64_from_bytes(&b[size_of::<u64>()..]).unwrap())
                                    .collect::<Vec<_>>(),
                                state
                                    .3
                                    .iter()
                                    .map(|b| utils::u64_from_bytes(&b[size_of::<u64>()..]).unwrap())
                                    .collect::<Vec<_>>()
                            );
                            */

                            last_roomstates.insert(current_room.clone(), current_sstatehash);
                        }
                        current_state = HashSet::new();
                        current_sstatehash = sstatehash;

                        let event_id = db
                            .rooms
                            .shorteventid_eventid
                            .get(&seventid)
                            .unwrap()
                            .unwrap();
                        let event_id =
                            EventId::try_from(utils::string_from_bytes(&event_id).unwrap())
                                .unwrap();
                        let pdu = db.rooms.get_pdu(&event_id).unwrap().unwrap();

                        if Some(&pdu.room_id) != current_room.as_ref() {
                            current_room = Some(pdu.room_id.clone());
                        }
                    }

                    let mut val = sstatekey;
                    val.extend_from_slice(&seventid);
                    current_state.insert(val);
                }

                db.globals.bump_database_version(6)?;

                println!("Migration: 5 -> 6 finished");
            }

            if db.globals.database_version()? < 7 {
                // Generate short room ids for all rooms
                for (room_id, _) in db.rooms.roomid_shortstatehash.iter() {
                    let shortroomid = db.globals.next_count()?.to_be_bytes();
                    db.rooms.roomid_shortroomid.insert(&room_id, &shortroomid)?;
                    db.rooms.shortroomid_roomid.insert(&shortroomid, &room_id)?;
                }
                // Update pduids db layout
                for (key, v) in db.rooms.pduid_pdu.iter() {
                    let mut parts = key.splitn(2, |&b| b == 0xff);
                    let room_id = parts.next().unwrap();
                    let count = parts.next().unwrap();

                    let short_room_id = db.rooms.roomid_shortroomid.get(&room_id)?.unwrap();

                    let mut new_key = short_room_id;
                    new_key.extend_from_slice(count);

                    println!("{:?}", new_key);
                }

                // Update tokenids db layout
                for (key, _) in db.rooms.tokenids.iter() {
                    let mut parts = key.splitn(4, |&b| b == 0xff);
                    let room_id = parts.next().unwrap();
                    let word = parts.next().unwrap();
                    let _pdu_id_room = parts.next().unwrap();
                    let pdu_id_count = parts.next().unwrap();

                    let short_room_id = db.rooms.roomid_shortroomid.get(&room_id)?.unwrap();
                    let mut new_key = short_room_id;
                    new_key.extend_from_slice(word);
                    new_key.push(0xff);
                    new_key.extend_from_slice(pdu_id_count);
                    println!("{:?}", new_key);
                }

                db.globals.bump_database_version(7)?;

                println!("Migration: 6 -> 7 finished");
            }

            panic!();
        }

        let guard = db.read().await;

        // This data is probably outdated
        guard.rooms.edus.presenceid_presence.clear()?;

        guard.admin.start_handler(Arc::clone(&db), admin_receiver);
        guard
            .sending
            .start_handler(Arc::clone(&db), sending_receiver);

        drop(guard);

        #[cfg(feature = "sqlite")]
        {
            Self::start_wal_clean_task(&db, &config).await;
            Self::start_spillover_reap_task(builder, &config).await;
        }

        Ok(db)
    }

    #[cfg(feature = "conduit_bin")]
    pub async fn start_on_shutdown_tasks(db: Arc<TokioRwLock<Self>>, shutdown: Shutdown) {
        use tracing::info;

        tokio::spawn(async move {
            shutdown.await;

            info!(target: "shutdown-sync", "Received shutdown notification, notifying sync helpers...");

            db.read().await.globals.rotate.fire();
        });
    }

    pub async fn watch(&self, user_id: &UserId, device_id: &DeviceId) {
        let userid_bytes = user_id.as_bytes().to_vec();
        let mut userid_prefix = userid_bytes.clone();
        userid_prefix.push(0xff);

        let mut userdeviceid_prefix = userid_prefix.clone();
        userdeviceid_prefix.extend_from_slice(device_id.as_bytes());
        userdeviceid_prefix.push(0xff);

        let mut futures = FuturesUnordered::new();

        // Return when *any* user changed his key
        // TODO: only send for user they share a room with
        futures.push(
            self.users
                .todeviceid_events
                .watch_prefix(&userdeviceid_prefix),
        );

        futures.push(self.rooms.userroomid_joined.watch_prefix(&userid_prefix));
        futures.push(
            self.rooms
                .userroomid_invitestate
                .watch_prefix(&userid_prefix),
        );
        futures.push(self.rooms.userroomid_leftstate.watch_prefix(&userid_prefix));
        futures.push(
            self.rooms
                .userroomid_notificationcount
                .watch_prefix(&userid_prefix),
        );
        futures.push(
            self.rooms
                .userroomid_highlightcount
                .watch_prefix(&userid_prefix),
        );

        // Events for rooms we are in
        for room_id in self.rooms.rooms_joined(user_id).filter_map(|r| r.ok()) {
            let roomid_bytes = room_id.as_bytes().to_vec();
            let mut roomid_prefix = roomid_bytes.clone();
            roomid_prefix.push(0xff);

            // PDUs
            futures.push(self.rooms.pduid_pdu.watch_prefix(&roomid_prefix));

            // EDUs
            futures.push(
                self.rooms
                    .edus
                    .roomid_lasttypingupdate
                    .watch_prefix(&roomid_bytes),
            );

            futures.push(
                self.rooms
                    .edus
                    .readreceiptid_readreceipt
                    .watch_prefix(&roomid_prefix),
            );

            // Key changes
            futures.push(self.users.keychangeid_userid.watch_prefix(&roomid_prefix));

            // Room account data
            let mut roomuser_prefix = roomid_prefix.clone();
            roomuser_prefix.extend_from_slice(&userid_prefix);

            futures.push(
                self.account_data
                    .roomusertype_roomuserdataid
                    .watch_prefix(&roomuser_prefix),
            );
        }

        let mut globaluserdata_prefix = vec![0xff];
        globaluserdata_prefix.extend_from_slice(&userid_prefix);

        futures.push(
            self.account_data
                .roomusertype_roomuserdataid
                .watch_prefix(&globaluserdata_prefix),
        );

        // More key changes (used when user is not joined to any rooms)
        futures.push(self.users.keychangeid_userid.watch_prefix(&userid_prefix));

        // One time keys
        futures.push(
            self.users
                .userid_lastonetimekeyupdate
                .watch_prefix(&userid_bytes),
        );

        futures.push(Box::pin(self.globals.rotate.watch()));

        // Wait until one of them finds something
        futures.next().await;
    }

    #[tracing::instrument(skip(self))]
    pub async fn flush(&self) -> Result<()> {
        let start = std::time::Instant::now();

        let res = self._db.flush();

        debug!("flush: took {:?}", start.elapsed());

        res
    }

    #[cfg(feature = "sqlite")]
    #[tracing::instrument(skip(self))]
    pub fn flush_wal(&self) -> Result<()> {
        self._db.flush_wal()
    }

    #[cfg(feature = "sqlite")]
    #[tracing::instrument(skip(engine, config))]
    pub async fn start_spillover_reap_task(engine: Arc<Engine>, config: &Config) {
        let fraction = config.sqlite_spillover_reap_fraction.clamp(0.01, 1.0);
        let interval_secs = config.sqlite_spillover_reap_interval_secs as u64;

        let weak = Arc::downgrade(&engine);

        tokio::spawn(async move {
            use tokio::time::interval;

            use std::{sync::Weak, time::Duration};

            let mut i = interval(Duration::from_secs(interval_secs));

            loop {
                i.tick().await;

                if let Some(arc) = Weak::upgrade(&weak) {
                    arc.reap_spillover_by_fraction(fraction);
                } else {
                    break;
                }
            }
        });
    }

    #[cfg(feature = "sqlite")]
    #[tracing::instrument(skip(lock, config))]
    pub async fn start_wal_clean_task(lock: &Arc<TokioRwLock<Self>>, config: &Config) {
        use tokio::time::{interval, timeout};

        #[cfg(unix)]
        use tokio::signal::unix::{signal, SignalKind};
        use tracing::info;

        use std::{
            sync::Weak,
            time::{Duration, Instant},
        };

        let weak: Weak<TokioRwLock<Database>> = Arc::downgrade(&lock);

        let lock_timeout = Duration::from_secs(config.sqlite_wal_clean_second_timeout as u64);
        let timer_interval = Duration::from_secs(config.sqlite_wal_clean_second_interval as u64);
        let do_timer = config.sqlite_wal_clean_timer;

        tokio::spawn(async move {
            let mut i = interval(timer_interval);
            #[cfg(unix)]
            let mut s = signal(SignalKind::hangup()).unwrap();

            loop {
                #[cfg(unix)]
                tokio::select! {
                    _ = i.tick(), if do_timer => {
                        info!(target: "wal-trunc", "Timer ticked")
                    }
                    _ = s.recv() => {
                        info!(target: "wal-trunc", "Received SIGHUP")
                    }
                };
                #[cfg(not(unix))]
                if do_timer {
                    i.tick().await;
                    info!(target: "wal-trunc", "Timer ticked")
                } else {
                    // timer disabled, and there's no concept of signals on windows, bailing...
                    return;
                }
                if let Some(arc) = Weak::upgrade(&weak) {
                    info!(target: "wal-trunc", "Rotating sync helpers...");
                    // This actually creates a very small race condition between firing this and trying to acquire the subsequent write lock.
                    // Though it is not a huge deal if the write lock doesn't "catch", as it'll harmlessly time out.
                    arc.read().await.globals.rotate.fire();

                    info!(target: "wal-trunc", "Locking...");
                    let guard = {
                        if let Ok(guard) = timeout(lock_timeout, arc.write()).await {
                            guard
                        } else {
                            info!(target: "wal-trunc", "Lock failed in timeout, canceled.");
                            continue;
                        }
                    };
                    info!(target: "wal-trunc", "Locked, flushing...");
                    let start = Instant::now();
                    if let Err(e) = guard.flush_wal() {
                        error!(target: "wal-trunc", "Errored: {}", e);
                    } else {
                        info!(target: "wal-trunc", "Flushed in {:?}", start.elapsed());
                    }
                } else {
                    break;
                }
            }
        });
    }
}

pub struct DatabaseGuard(OwnedRwLockReadGuard<Database>);

impl Deref for DatabaseGuard {
    type Target = OwnedRwLockReadGuard<Database>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for DatabaseGuard {
    type Error = ();

    async fn from_request(req: &'r Request<'_>) -> rocket::request::Outcome<Self, ()> {
        let db = try_outcome!(req.guard::<&State<Arc<TokioRwLock<Database>>>>().await);

        Ok(DatabaseGuard(Arc::clone(&db).read_owned().await)).or_forward(())
    }
}

impl From<OwnedRwLockReadGuard<Database>> for DatabaseGuard {
    fn from(val: OwnedRwLockReadGuard<Database>) -> Self {
        Self(val)
    }
}
