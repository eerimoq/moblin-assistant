use log::info;
use tokio::time::{sleep, Duration};

use crate::{Streamer, Streamers};

pub fn start_periodic_cleanup(streamers: Streamers) {
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_mins(15)).await;
            streamers.lock().await.retain(|_, streamer| {
                let Streamer::Disconnected(ref streamer) = streamer else {
                    return true;
                };
                streamer.time.elapsed() < Duration::from_hours(1)
            });
            info!("Number of streamers: {}", streamers.lock().await.len());
        }
    });
}
