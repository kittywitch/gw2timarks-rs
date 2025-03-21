use {
    crate::*,
    crate::bhtimer,
    crate::bhtimer::*,
    glob::{glob, Paths},
    glam::f32::Vec3,
    nexus::data_link::{read_mumble_link, MumbleLink},
    std::{
        collections::HashMap,
        fs::File,
        path::PathBuf,
    },
    tokio::{
        runtime, select,
        sync::mpsc::Receiver,
    },
};

#[derive(Debug, Clone)]
pub struct TaimiState {
    pub addon_dir: PathBuf,
    pub cached_identity: Option<MumbleIdentityUpdate>,
    pub cached_link: Option<MumbleLink>,
    pub timers: HashMap<String, TimerFile>,
    // TODO: Refactor to be a hashmap of ID to pointer to timerfile
    // instead of any use of timer_id, use the Arc as a shared reference
    //
    // * no longer have to worry about .clone()
    // * don't have to worry about lifetimes thanks to arc
    // THANKS ARC <3
    //map_id_to_timers: HashMap<u32, Vec<Arc<TimerFile>>,
    //category_to_timers: HashMap<String, Vec<Arc<TimerFile>>,
    pub map_id_to_timer_ids: HashMap<u32, Vec<String>>,
    pub category_to_timer_ids: HashMap<String, Vec<String>>,
    pub map_id: Option<u32>,
    pub player_position: Option<Vec3>,
    pub timers_for_map: Vec<String>,
    // TODO: This should be...
    // current_timers: Vec<TimerMachine>
    pub starts_to_check: HashMap<String, TimerPhase>,
}
impl TaimiState {
    pub fn load(mut tm_receiver: Receiver<TaimiThreadEvent>, addon_dir: PathBuf) {
        let mut state = TaimiState {
            addon_dir,
            cached_identity: None,
            cached_link: None,
            timers: Default::default(),
            map_id_to_timer_ids: Default::default(),
            category_to_timer_ids: Default::default(),
            map_id: None,
            player_position: None,
            timers_for_map: Default::default(),
            starts_to_check: Default::default(),
        };

        let evt_loop = async move {
            state.setup_timers().await;
            let mut taimi_interval = tokio::time::interval(tokio::time::Duration::from_millis(250));
            let mut mumblelink_interval = tokio::time::interval(tokio::time::Duration::from_millis(20));
            loop {
                select! {
                    evt = tm_receiver.recv() => match evt {
                        Some(evt) => {
                            match state.handle_event(evt).await {
                                Ok(true) => (),
                                Ok(false) => break,
                                Err(error) => {
                                    log::error!("Error! {}", error)
                                }
                            }
                        },
                        None => {
                            break
                        },
                    },
                    _ = mumblelink_interval.tick() => {
                        let _ = state.mumblelink_tick().await;
                    },
                    _ = taimi_interval.tick() => {
                        let _ = state.tick().await;
                    },
                }
            }
        };
        let rt = match runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(error) => {
                log::error!("Error! {}", error);
                return;
            }
        };
        rt.block_on(evt_loop);
    }
    async fn load_timer_file(&self, path: PathBuf) -> anyhow::Result<bhtimer::TimerFile> {
        log::info!("Attempting to load the timer file at '{path:?}'.");
        let file = File::open(path)?;
        let timer_data: TimerFile = serde_jsonrc::from_reader(file)?;
        Ok(timer_data)
    }

    async fn get_paths(&self, path: &PathBuf) -> anyhow::Result<Paths> {
        let timer_paths: Paths = glob(path.to_str().expect("Pattern is unparseable"))?;
        Ok(timer_paths)
    }

    async fn load_timer_files(&self) -> Vec<bhtimer::TimerFile> {
        let mut timers = Vec::new();
        let glob_str = self.addon_dir.join("*.bhtimer");
        log::info!("Path to load timer files is '{glob_str:?}'.");
        let timer_paths: Paths = self.get_paths(&glob_str).await.unwrap();
        for path in timer_paths {
            let path = path.expect("Path illegible!");
            match self.load_timer_file(path.clone()).await {
                Ok(data) => {
                    log::info!("Successfully loaded the timer file at '{path:?}'.");

                    timers.push(data);
                }
                Err(error) => log::warn!("Failed to load the timer file at '{path:?}': {error}."),
            };
        }
        timers
    }

    async fn setup_timers(&mut self) {
        log::info!("Preparing to setup timers");
        let timers = self.load_timer_files().await;

        for timer in timers {
            let timer_held = timer.clone();
            // Handle map_id to timer_id
            if !self.map_id_to_timer_ids.contains_key(&timer.map_id) {
                self.map_id_to_timer_ids
                    .insert(timer.map_id.clone(), Vec::new());
            }
            if let Some(val) = self.map_id_to_timer_ids.get_mut(&timer.map_id) {
                val.push(timer.id.clone());
            };
            // Handle category to timer_id list
            if !self.category_to_timer_ids.contains_key(&timer.category) {
                self.category_to_timer_ids
                    .insert(timer.category.clone(), Vec::new());
            }
            if let Some(val) = self.category_to_timer_ids.get_mut(&timer.category) {
                val.push(timer.id.clone());
            };
            // Handle id to timer file allocation
            log::info!(
                "Set up {0} for map {1}, category {2}",
                timer.id,
                timer.map_id,
                timer.category
            );
            self.timers.insert(timer.id, timer_held);
        }
    }

    // TODO: refactor code such that the start triggers are handled as part of the
    // TimerMachine, where we check if it is OnMap and untriggered...
    // The code for checking sphere/cuboid regions should be built into the actual TimerMachine
    // This avoids mutating a collection and allows us to reckon with these things as checking the
    // Enum value
    async fn tick(&mut self) -> anyhow::Result<()> {
        let mut started_ids = Vec::new();
        for (timer_id, start_phase) in &self.starts_to_check {
            use bhtimer::TimerTriggerType::*;
            let start_trigger = &start_phase.start;
            match &start_trigger.kind {
                Location => {
                    let p1 = start_trigger.position().unwrap();
                    if let Some(player) = self.player_position {
                        // Check a sphere
                        if let Some(radius) = start_trigger.radius {
                            if p1.distance(player) < radius {
                                log::info!(
                                    "Player is within the spherical boundary for '{}'.",
                                    start_phase.name
                                );
                                started_ids.push(timer_id.clone());
                            }
                        }
                        // Check a cuboid
                        if let Some(p2) = start_trigger.antipode() {
                            let mins = p1.min(p2);
                            let maxs = p1.max(p2);
                            let min_cmp = player.cmpge(mins);
                            let max_cmp = player.cmple(maxs);
                            let player_in_area = min_cmp.all() && max_cmp.all();
                            if player_in_area {
                                log::info!(
                                    "Player is within the cuboid boundary for '{}'.",
                                    start_phase.name
                                );
                                started_ids.push(timer_id.clone());
                            }
                        }
                    }
                }
                Key => (),
            }
        }
        for started_id in started_ids {
            self.starts_to_check.remove(&started_id);
        }
        Ok(())
    }

    async fn mumblelink_tick(&mut self) -> anyhow::Result<()> {
        self.cached_link = read_mumble_link();
        if let Some(link) = &self.cached_link {
            self.player_position = Some(Vec3::from_array(link.avatar.position));
        };
        Ok(())
    }

    async fn handle_event(&mut self, event: TaimiThreadEvent) -> anyhow::Result<bool> {
        use TaimiThreadEvent::*;
        match event {
            MumbleIdentityUpdated(identity) => {
                if self.map_id != Some(identity.map_id) {
                    match self.map_id {
                        Some(map_id) => log::info!(
                            "User has changed map from {0} to {1}",
                            map_id,
                            identity.map_id
                        ),
                        None => log::info!("User's map is {0}", identity.map_id),
                    }
                    self.map_id = Some(identity.map_id);
                    let map_id_local = &self.map_id.unwrap();
                    if self.map_id_to_timer_ids.contains_key(map_id_local) {
                        let timers_for_map = &self.map_id_to_timer_ids[map_id_local];
                        let timers_list = timers_for_map.join(", ");
                        let mut starts_to_check = HashMap::new();
                        for timer_id in timers_for_map {
                            let timer = &self.timers[timer_id];
                            let start_phase = &timer.phases[0];
                            starts_to_check.insert(timer_id.clone(), start_phase.clone());
                        }
                        self.starts_to_check = starts_to_check;
                        self.timers_for_map = timers_for_map.to_vec();
                        log::info!("Timers found for map {0}: {1}", map_id_local, timers_list);
                    } else {
                        self.starts_to_check = HashMap::new();
                        self.timers_for_map = Vec::new();
                        log::info!("No timers found for map {0}.", map_id_local);
                    }
                }
                self.cached_identity = Some(identity);
            }
            Quit => return Ok(false),
            _ => (),
        }
        Ok(true)
    }
}


#[derive(Debug, Clone)]
pub enum TaimiThreadEvent {
    MumbleIdentityUpdated(MumbleIdentityUpdate),
    Quit,
}
