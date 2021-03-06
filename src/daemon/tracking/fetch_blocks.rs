use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{Future, Async};
use futures_cpupool::CpuFuture;
use valuable_futures::{StateMachine, Async as VAsync};
use tokio_core::reactor::Timeout;
use tk_easyloop::timeout;

use blocks::BlockHash;
use proto::{RequestFuture, GetBlock, GetBlockResponse};
use proto::{RequestClient};
use tracking::progress::{Downloading, Block};
use tracking::Subsystem;
use disk::{self, Image};
use mask::Mask;


const FETCH_DEADLINE: u64 = 3600_000;  // one hour
const RETRY_CLUSTER_FAILURE: u64 = 120_000;  // Two minutes
const RETRY_INTERVAL: u64 = 2000;  // 2 seconds
const CONCURRENCY: usize = 10;


pub struct FetchBlocks {
    sys: Subsystem,
    downloading: Arc<Downloading>,
    image: Arc<Image>,
    futures: VecDeque<FetchBlock>,
    retry_timeout: Option<Timeout>,
    last_okay: Instant,
    deadline: Timeout,
}

pub enum FetchBlock {
    // TODO(tailhook) we mix incoming and outgoing connection in addr
    Fetching(Block, u8, SocketAddr, RequestFuture<GetBlockResponse>),
    Writing(CpuFuture<(), disk::Error>),
}


impl FetchBlocks {
    pub fn new(image: &Arc<Image>, down: &Arc<Downloading>,
            sys: &Subsystem)
        -> FetchBlocks
    {
        FetchBlocks {
            sys: sys.clone(),
            image: image.clone(),
            downloading: down.clone(),
            futures: VecDeque::new(),
            retry_timeout: None,
            last_okay: Instant::now(),
            deadline: timeout(Duration::from_millis(FETCH_DEADLINE)),
        }
    }
}

impl StateMachine for FetchBlock {
    type Supply = FetchBlocks;
    type Item = ();
    type Error = ();
    fn poll(self, ctx: &mut FetchBlocks) -> Result<VAsync<(), Self>, ()> {
        use self::FetchBlock::*;
        let mut state = self;
        loop {
            state = match state {
                Fetching(blk, slice, addr, mut f) => {
                    match f.poll() {
                        Ok(Async::NotReady) => {
                            return Ok(VAsync::NotReady(
                                Fetching(blk, slice, addr, f)));
                        }
                        Ok(Async::Ready(data)) => {
                            if BlockHash::hash_bytes(&data.data) == blk.hash {
                                for s in ctx.downloading.slices().iter_mut() {
                                    if s.index == slice {
                                        s.in_progress -= 1;
                                    }
                                }
                                let data = Arc::new(data.data);
                                ctx.last_okay = Instant::now();
                                ctx.downloading.report_block(&data);
                                Writing(ctx.sys.disk.write_block(
                                    ctx.image.clone(),
                                    blk.path.clone(),
                                    blk.offset,
                                    data))
                            } else {
                                error!("Wrong checksum \
                                    when reading block {:?}",
                                    blk);
                                for s in ctx.downloading.slices().iter_mut() {
                                    if s.index == slice {
                                        s.in_progress -= 1;
                                        s.blocks.push_back(blk);
                                        s.failures.add_failure(addr);
                                        break;
                                    }
                                }
                                return Err(());
                            }
                        }
                        Err(e) => {
                            for s in ctx.downloading.slices().iter_mut() {
                                if s.index == slice {
                                    s.in_progress -= 1;
                                    s.blocks.push_back(blk);
                                    s.failures.add_failure(addr);
                                    break;
                                }
                            }
                            error!("Block fetch error: {}", e);
                            return Err(());
                        }
                    }
                }
                Writing(mut f) => {
                    match f.poll() {
                        Ok(Async::Ready(())) => {
                            // TODO(tailhook) mark it
                            return Ok(VAsync::Ready(()));
                        }
                        Ok(Async::NotReady) => {
                            return Ok(VAsync::NotReady(Writing(f)));
                        }
                        Err(e) => {
                            // TODO(tailhook) better message
                            error!("Block write error: {}", e);
                            // TODO(tailhook) sleep and retry?
                            // or is it fatal?
                            ::std::process::exit(102);
                            // return Err(());
                        }
                    }
                }
            }
        }
    }
}


impl Future for FetchBlocks {
    type Item = ();
    type Error = ();
    fn poll(&mut self) -> Result<Async<()>, ()> {
        use self::FetchBlock::Fetching;

        if let Some(mut timeout) = self.retry_timeout.take() {
            match timeout.poll().expect("timeout never fails") {
                Async::Ready(()) => {},
                Async::NotReady => {
                    self.retry_timeout = Some(timeout);
                    return Ok(Async::NotReady);
                }
            }
        }
        'outer: loop {
            for _ in 0 .. self.futures.len() {
                match self.futures.pop_front() {
                    Some(f) => match f.poll(self) {
                        Ok(VAsync::NotReady(x)) => self.futures.push_back(x),
                        Ok(VAsync::Ready(())) => {}
                        Err(()) => {}
                    },
                    None => unreachable!(),
                }
            }
            self.downloading.slices().retain(|s| {
                if s.in_progress > 0 || s.blocks.len() > 0 {
                    true
                } else {
                    let ref dw = self.downloading;
                    dw.report_slice(s.index as usize);
                    self.sys.peers.notify_progress(&dw.virtual_path,
                        &dw.image_id, dw.mask.get(),
                        self.sys.remote.has_image_source(&dw.image_id));
                    false
                }
            });
            if self.futures.len() == 0 &&
                self.downloading.slices().len() == 0
            {
                return Ok(Async::Ready(()));
            } else if self.deadline.poll().expect("timers don't fail")
                .is_ready()
            {
                error!("Deadline reached while fetching blocks");
                return Err(());
            } else if self.futures.len() >= CONCURRENCY {
                return Ok(Async::NotReady);
            }
            let mut new = 0;
            for s in self.downloading.slices().iter_mut() {
                while let Some(blk) = s.blocks.pop_front() {
                    // TODO(tailhook) Try peer connections
                    let conn = self.sys.tracking
                        .get_connection_by_mask(
                            &self.downloading.virtual_path,
                            &self.downloading.image_id,
                            Mask::slice_bit(s.index as usize), &s.failures);
                    if let Some(conn) = conn {
                        let req = conn.request(GetBlock {
                            hash: blk.hash.clone(),
                            hint: Some((
                                self.downloading.virtual_path.clone(),
                                (*blk.path).clone(),
                                blk.offset,
                            )),
                        });
                        let f = Fetching(blk, s.index, conn.addr(), req);
                        new += 1;
                        self.futures.push_back(f);
                        s.in_progress += 1;
                        if self.futures.len() > CONCURRENCY {
                            continue 'outer;
                        }
                    } else {
                        s.blocks.push_back(blk);
                        break;
                    }
                }
            }
            if new > 0 {
                // must poll new futures
                continue;
            }
            if self.futures.len() == 0 {
                self.downloading.notify_stalled();
                info!("Nowhere to fetch some chunks of {} at {:?}. \
                    Waiting...",
                    self.downloading.image_id,
                    self.downloading.virtual_path);
                let cretry = Duration::from_millis(RETRY_CLUSTER_FAILURE);
                if self.last_okay + cretry < Instant::now() {
                    let cstalled = self.sys.peers.check_stalled(
                        &self.downloading.virtual_path,
                        &self.downloading.image_id);
                    if cstalled {
                        error!("Noticed that all nodes on downloading \
                            {} to {:?} are stalled. \
                            This probably means that all sources which \
                            uploaded the image have been gone before \
                            upload was done. Canceling directory sync, \
                            so another client could initiated upload again.",
                            self.downloading.image_id,
                            self.downloading.virtual_path);
                        return Err(());
                    }
                }
                let mut t = timeout(Duration::from_millis(RETRY_INTERVAL));
                match t.poll().expect("timeout never fails") {
                    Async::Ready(()) => continue,
                    Async::NotReady => {}
                }
                self.retry_timeout = Some(t);
            }
            return Ok(Async::NotReady);
        }
    }
}
