//! The mixer: the set of live routes (patch cables). Any app can go to any
//! number of outputs, and any output can take any number of apps; Windows
//! mixes multiple routes into the same device for us.

use crate::outputs::Device;
use crate::routes::Route;
use crate::sources::AppSource;

#[derive(Default)]
pub struct Mixer {
    routes: Vec<Route>,
    next_id: u64,
}

impl Mixer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    pub fn find(&self, pid: u32, device_id: &str) -> Option<&Route> {
        self.routes.iter().find(|r| r.source.pid == pid && r.output.id == device_id)
    }

    /// Connect an app to an output. Returns the existing route's id if that
    /// pair is already connected.
    pub fn connect(&mut self, source: AppSource, output: Device, gain: f32) -> u64 {
        if let Some(r) = self.find(source.pid, &output.id) {
            return r.id;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.routes.push(Route::start(id, source, output, gain));
        id
    }

    /// Disconnect (and stop) a route.
    pub fn disconnect(&mut self, id: u64) {
        // Dropping the route stops its threads.
        self.routes.retain(|r| r.id != id);
    }

    /// Re-attach routes whose app has closed to a newly started instance of
    /// the same app (same executable name), keeping volume and mute.
    /// `apps` is the current app list. Returns how many routes moved.
    pub fn reattach(&mut self, apps: &[AppSource]) -> usize {
        let mut moved = 0;
        for i in 0..self.routes.len() {
            let r = &self.routes[i];
            if apps.iter().any(|a| a.pid == r.source.pid) {
                continue; // app still there
            }
            let Some(new_app) = apps.iter().find(|a| a.name.eq_ignore_ascii_case(&r.source.name)) else {
                continue; // not back yet
            };
            if self.find(new_app.pid, &r.output.id).is_some() {
                continue;
            }
            let (id, output, gain, muted) = (r.id, r.output.clone(), r.gain.get(), r.gain.is_muted());
            let fresh = Route::start(id, new_app.clone(), output, gain);
            if muted {
                fresh.gain.toggle_mute();
            }
            self.routes[i] = fresh; // old route is dropped (stopped) here
            moved += 1;
        }
        moved
    }

    /// Connect the pair if it isn't connected, disconnect it if it is.
    /// Returns true if the pair is now connected.
    pub fn toggle(&mut self, source: AppSource, output: Device, gain: f32) -> bool {
        match self.find(source.pid, &output.id).map(|r| r.id) {
            Some(id) => {
                self.disconnect(id);
                false
            }
            None => {
                self.connect(source, output, gain);
                true
            }
        }
    }
}
