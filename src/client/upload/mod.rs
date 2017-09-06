use std::collections::{HashMap, HashSet};
use std::io::{self, stdout, stderr};
use std::net::SocketAddr;
use std::process::exit;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use abstract_ns::Resolver;
use argparse::{ArgumentParser};
use dir_signature::{ScannerConfig, HashType, v1, get_hash};
use futures::future::{Future, Either, join_all, ok};
use futures::sync::oneshot::{channel, Sender};
use futures_cpupool::CpuPool;
use tk_easyloop;

mod options;

use name;
use ciruela::{Hash, VPath, MachineId};
use ciruela::time::to_ms;
use ciruela::proto::{SigData, sign};
use global_options::GlobalOptions;
use ciruela::proto::{Client, AppendDir, ReplaceDir, ImageInfo, BlockPointer};
use ciruela::proto::RequestClient;
use ciruela::proto::{Listener};
use ciruela::proto::message::Notification;

struct Progress {
    started: SystemTime,
    hosts_done: HashMap<MachineId, String>,
    ips_needed: HashSet<SocketAddr>,
    ids_needed: HashMap<MachineId, String>,
    hosts_errored: HashSet<SocketAddr>,
    done: Option<Sender<()>>,
}

struct Tracker(SocketAddr, Arc<Mutex<Progress>>);


impl Progress {
    fn hosts_done(&self) -> String {
        self.hosts_done.values().map(|x| &x[..]).collect::<Vec<_>>().join(", ")
    }
    fn add_ids(&mut self, hosts: HashMap<MachineId, String>) {
        for (id, hostname) in hosts {
            if !self.hosts_done.contains_key(&id) {
                self.ids_needed.insert(id, hostname);
            }
        }
    }
}

impl Listener for Tracker {
    fn notification(&self, n: Notification) {
        use ciruela::proto::message::Notification::*;
        match n {
            ReceivedImage(img) => {
                // TODO(tailhook) check image id and path
                let mut pro = self.1.lock().expect("progress is not poisoned");
                pro.hosts_done.insert(img.machine_id, img.hostname);
                if !img.forwarded {
                    pro.ips_needed.remove(&self.0);
                }
                if pro.ips_needed.len() == 0 {
                    info!("Fetched from {}", pro.hosts_done());
                    eprintln!("Fetched from all required hosts. {} total. \
                        Done in {} seconds.",
                        pro.hosts_done.len(),
                        SystemTime::now().duration_since(pro.started)
                            .unwrap().as_secs());
                    pro.done.take().map(|chan| {
                        chan.send(()).expect("sending done");
                    });
                } else {
                    eprint!("Fetched from ({}/{}) {}\r",
                        pro.hosts_done.len(),
                        pro.hosts_done.len() +
                            pro.ids_needed.len() + pro.ips_needed.len(),
                        pro.hosts_done());
                }
            }
            _ => {}
        }
    }
    fn closed(&self) {
        // TODO(tailhook) reconnect
        let mut pro = self.1.lock().expect("progress is not poisoted");
        if pro.done.is_some() {
            error!("Connection to {} is closed", self.0);
            pro.ips_needed.remove(&self.0);
            pro.hosts_errored.insert(self.0);
            if pro.ips_needed.len() == 0 {
                pro.done.take().map(|chan| {
                    chan.send(()).ok();
                });
            }
        }
    }
}

fn is_ok(pro: &Arc<Mutex<Progress>>) -> bool {
    pro.lock().expect("progress is ok").hosts_errored.len() == 0
}

fn do_upload(gopt: GlobalOptions, opt: options::UploadOptions)
    -> Result<bool, ()>
{
    let dir = opt.source_directory.clone().unwrap();
    let mut cfg = ScannerConfig::new();
    cfg.hash(HashType::blake2b_256());
    cfg.add_dir(&dir, "/");
    cfg.print_progress();

    let mut indexbuf = Vec::new();
    v1::scan(&cfg, &mut indexbuf)
        .map_err(|e| error!("Error scanning {:?}: {}", dir, e))?;

    let pool = CpuPool::new(gopt.threads);

    let image_id = get_hash(&mut io::Cursor::new(&indexbuf))
        .expect("hash valid in just created index")
        .into();
    let (blocks, block_size) = {
        let ref mut cur = io::Cursor::new(&indexbuf);
        let mut parser = v1::Parser::new(cur)
            .expect("just created index is valid");
        let header = parser.get_header();
        let block_size = header.get_block_size();
        let mut blocks = HashMap::new();
        for entry in parser.iter() {
            match entry.expect("just created index is valid") {
                v1::Entry::File { ref path, ref hashes, .. } => {
                    let path = Arc::new(path.to_path_buf());
                    for (idx, hash) in hashes.iter().enumerate() {
                        blocks.insert(Hash::new(hash), BlockPointer {
                            file: path.clone(),
                            offset: idx as u64 * block_size,
                        });
                    }
                }
                _ => {}
            }
        }
        (blocks, block_size)
    };
    let image_info = Arc::new(ImageInfo {
        image_id: image_id,
        block_size: block_size,
        index_data: indexbuf,
        location: dir.to_path_buf(),
        blocks: blocks,
    });
    let timestamp = SystemTime::now();
    let mut signatures = HashMap::new();
    for turl in &opt.target_urls {
        if signatures.contains_key(&turl.path[..]) {
            continue;
        }
        signatures.insert(&turl.path[..], sign(SigData {
            path: &turl.path,
            image: image_info.image_id.as_ref(),
            timestamp: to_ms(timestamp),
        }, &opt.private_keys));
    }
    let signatures = Arc::new(signatures);
    let (done_tx, done_rx) = channel();
    let done_rx = done_rx.shared();
    let progress = Arc::new(Mutex::new(Progress {
        started: SystemTime::now(),
        hosts_done: HashMap::new(),
        ips_needed: HashSet::new(),
        ids_needed: HashMap::new(),
        hosts_errored: HashSet::new(),
        done: Some(done_tx),
    }));
    let replace = opt.replace;

    tk_easyloop::run(|| {
        let resolver = name::resolver();
        join_all(
            opt.target_urls.iter()
            .map(move |turl| {
                let host = Arc::new(
                    format!("{}:{}", turl.host, gopt.destination_port));
                let host2 = host.clone();
                let host3 = host.clone();
                let host4 = host.clone();
                let image_info = image_info.clone();
                let pool = pool.clone();
                let signatures = signatures.clone();
                let done_rx = done_rx.clone();
                let progress = progress.clone();
                resolver.resolve(&host)
                .map_err(move |e| {
                    error!("Error resolving host {}: {}", host2, e);
                })
                .and_then(move |addr| name::pick_hosts(&*host3, addr))
                .and_then(move |names| {
                    let done_rx = done_rx.clone();
                    let progress = progress.clone();
                    join_all(
                        names.iter()
                        .map(move |&addr| {
                            let turl = turl.clone();
                            let signatures = signatures.clone();
                            let host = host4.clone();
                            let image_info = image_info.clone();
                            let pool = pool.clone();
                            let progress = progress.clone();
                            let progress2 = progress.clone();
                            let progress3 = progress.clone();
                            let tracker = Tracker(addr,
                                                  progress.clone());
                            progress.lock().expect("progress is ok")
                                .ips_needed.insert(addr);
                            let done_rx = done_rx.clone();
                            Client::spawn(addr, &host, &pool, tracker)
                            .and_then(move |mut cli| {
                                info!("Connected to {}", addr);
                                cli.register_index(&image_info);
                                if replace {
                                    Either::A(cli.request(ReplaceDir {
                                        image: image_info.image_id.clone(),
                                        timestamp: timestamp,
                                        old_image: None,
                                        signatures: signatures
                                            .get(&turl.path[..]).unwrap()
                                            .clone(),
                                        path: VPath::from(turl.path),
                                    })
                                    .map(move |resp| {
                                        info!("Response from {}: {:?}",
                                            addr, resp.accepted);
                                        progress3.lock()
                                            .expect("progress is ok")
                                            .add_ids(resp.hosts);
                                        resp.accepted
                                    })
                                    .map_err(|e|
                                        error!("Request error: {}", e)))
                                } else {
                                    Either::B(cli.request(AppendDir {
                                        image: image_info.image_id.clone(),
                                        timestamp: timestamp,
                                        signatures: signatures
                                            .get(&turl.path[..]).unwrap()
                                            .clone(),
                                        path: VPath::from(turl.path),
                                    })
                                    .map(move |resp| {
                                        info!("Response from {}: {:?}",
                                            addr, resp.accepted);
                                        progress3.lock()
                                            .expect("progress is ok")
                                            .add_ids(resp.hosts);
                                        resp.accepted
                                    })
                                    .map_err(|e|
                                        error!("Request error: {}", e)))
                                }
                            })
                            .and_then(move |accepted| {
                                if !accepted {
                                    progress.lock().expect("progress is ok")
                                        .ips_needed.remove(&addr);
                                    error!("Upload rejected by {} / {}",
                                        host, addr);
                                    Either::A(ok(false))
                                } else {
                                    Either::B(done_rx.clone()
                                        .then(move |v| match v {
                                            Ok(_) => Ok(is_ok(&progress)),
                                            Err(_) => {
                                                debug!("Connection closed \
                                                        before all \
                                                        notifications \
                                                        are received");
                                                Ok(false)
                                            }
                                        }))
                                }
                            })
                            .then(move |res| match res {
                                Ok(x) => Ok(x),
                                Err(()) => {
                                    progress2.lock().expect("progress is ok")
                                        .ips_needed.remove(&addr);
                                    // Always succeed, for now, so join will
                                    // drive all the futures, even if one
                                    // failed
                                    error!("Address {} has failed.", addr);
                                    Ok(false)
                                }
                            })
                        })
                        .collect::<Vec<_>>() // to unrefer "names"
                    )
                    .map(|results: Vec<bool>| results.iter().any(|x| *x))
                })
            })
            .collect::<Vec<_>>()
        )
        .map(|results:Vec<bool>| results.iter().all(|x| *x))
    })
}


pub fn cli(mut gopt: GlobalOptions, mut args: Vec<String>) -> ! {
    let mut opt = options::UploadOptions::new();
    {
        let mut ap = ArgumentParser::new();
        opt.define(&mut ap);
        gopt.define(&mut ap);
        args.insert(0, String::from("ciruela upload"));
        match ap.parse(args, &mut stdout(), &mut stderr()) {
            Ok(()) => {}
            Err(code) => exit(code),
        }
    }
    let opt = match opt.finalize() {
        Ok(opt) => opt,
        Err(code) => exit(code),
    };
    match do_upload(gopt, opt) {
        Ok(true) => exit(0),
        Ok(false) => exit(1),
        Err(()) => exit(2),
    }
}
