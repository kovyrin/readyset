use channel::tcp::TcpSender;
use channel::poll::{PollEvent, PollingLoop};
use distributary::{Blender, CoordinationMessage, CoordinationPayload};
use distributary::Index as DomainIndex;
use slog::Logger;
use std::io;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct WorkerStatus {
    healthy: bool,
    last_heartbeat: Instant,
    sender: Option<Arc<Mutex<TcpSender<CoordinationMessage>>>>,
}

impl WorkerStatus {
    pub fn new(sender: Arc<Mutex<TcpSender<CoordinationMessage>>>) -> Self {
        WorkerStatus {
            healthy: true,
            last_heartbeat: Instant::now(),
            sender: Some(sender),
        }
    }
}

pub struct Controller {
    listen_addr: String,
    listen_port: u16,

    log: Logger,

    blender: Arc<Mutex<Blender>>,
    workers: HashMap<SocketAddr, WorkerStatus>,

    heartbeat_every: Duration,
    healthcheck_every: Duration,
    last_checked_workers: Instant,
}

impl Controller {
    pub fn new(
        listen_addr: &str,
        port: u16,
        heartbeat_every: Duration,
        healthcheck_every: Duration,
        log: Logger,
    ) -> Controller {
        let mut blender = Blender::new();
        blender.log_with(log.clone());

        Controller {
            listen_addr: String::from(listen_addr),
            listen_port: port,
            log: log,
            blender: Arc::new(Mutex::new(blender)),
            workers: HashMap::new(),
            heartbeat_every: heartbeat_every,
            healthcheck_every: healthcheck_every,
            last_checked_workers: Instant::now(),
        }
    }

    pub fn get_blender(&self) -> Arc<Mutex<Blender>> {
        self.blender.clone()
    }

    /// Listen for workers to connect
    pub fn listen(&mut self) {
        use channel::poll::ProcessResult;
        use mio::net::TcpListener;
        use std::str::FromStr;

        let listener = TcpListener::bind(&SocketAddr::from_str(
            &format!("{}:{}", self.listen_addr, self.listen_port),
        ).unwrap()).unwrap();

        let mut pl: PollingLoop<CoordinationMessage> = PollingLoop::from_listener(listener);
        pl.run_polling_loop(|e| {
            match e {
                PollEvent::Process(ref msg) => {
                    trace!(self.log, "Received {:?}", msg);
                    match self.handle(msg) {
                        Ok(_) => (),
                        Err(e) => error!(self.log, "failed to handle message {:?}: {:?}", msg, e),
                    }
                }
                PollEvent::ResumePolling(timeout) => *timeout = Some(self.healthcheck_every),
                PollEvent::Timeout => (),
            }

            self.check_worker_liveness();

            ProcessResult::KeepPolling
        });
    }

    fn check_worker_liveness(&mut self) {
        if self.last_checked_workers.elapsed() > self.healthcheck_every {
            for (addr, ws) in self.workers.iter_mut() {
                if ws.healthy && ws.last_heartbeat.elapsed() > self.heartbeat_every * 3 {
                    warn!(self.log, "worker at {:?} has failed!", addr);
                    ws.healthy = false;
                }
            }
            self.last_checked_workers = Instant::now();
        }
    }

    fn handle(&mut self, msg: &CoordinationMessage) -> Result<(), io::Error> {
        match msg.payload {
            CoordinationPayload::Register(ref remote) => self.handle_register(msg, remote),
            CoordinationPayload::Heartbeat => self.handle_heartbeat(msg),
            CoordinationPayload::DomainBooted(ref domain, ref addr) => {
                self.handle_domain_booted(msg, domain, addr)
            }
            _ => unimplemented!(),
        }
    }

    fn handle_domain_booted(
        &mut self,
        msg: &CoordinationMessage,
        domain: &(DomainIndex, usize),
        addr: &SocketAddr,
    ) -> Result<(), io::Error> {
        use std::str::FromStr;

        // rewrite message source to be from the controller
        let mut fwd_msg = msg.clone();
        fwd_msg.source =
            SocketAddr::from_str(&format!("{}:{}", self.listen_addr, self.listen_port)).unwrap();

        // notify ChannelCoordinators on other workers about this new domain
        for (worker, mut status) in &mut self.workers {
            if *worker == msg.source {
                continue;
            }
            if status.healthy {
                let mut s = status.sender.as_mut().unwrap().lock().unwrap();
                s.send(fwd_msg.clone()).unwrap();
            }
        }
        Ok(())
    }

    fn handle_register(
        &mut self,
        msg: &CoordinationMessage,
        remote: &SocketAddr,
    ) -> Result<(), io::Error> {
        info!(
            self.log,
            "new worker registered from {:?}, which listens on {:?}",
            msg.source,
            remote
        );

        let sender = Arc::new(Mutex::new(TcpSender::connect(remote, None)?));
        let ws = WorkerStatus::new(sender.clone());
        self.workers.insert(msg.source.clone(), ws);

        let mut b = self.blender.lock().unwrap();
        b.add_worker(msg.source, sender);

        Ok(())
    }

    fn handle_heartbeat(&mut self, msg: &CoordinationMessage) -> Result<(), io::Error> {
        match self.workers.get_mut(&msg.source) {
            None => {
                crit!(
                    self.log,
                    "got heartbeat for unknown worker {:?}",
                    msg.source
                )
            }
            Some(ref mut ws) => {
                ws.last_heartbeat = Instant::now();
            }
        }

        Ok(())
    }
}
