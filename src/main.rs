use anyhow::Result;
use ffmpeg_sidecar::{command::FfmpegCommand}; // `IterExt` gives us `.filter_frames()`
use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use make87_messages::image::uncompressed::ImageRgb888;
use make87_messages::core::Header;
use make87_messages::google::protobuf::Timestamp;
use make87;

type LatestFrame = Arc<Mutex<Option<ImageRgb888>>>;

fn spawn_ffmpeg_reader(rtsp_url: &str, latest: LatestFrame) -> Result<()> {
    // Build the command --------------------------------------------
    let mut child = FfmpegCommand::new()
        // .args(["-fflags", "nobuffer"])              // minimise internal buffering
        .args(["-rtsp_transport", "tcp"])            // reliable transport
        .input(rtsp_url)                             // RTSP source  :contentReference[oaicite:0]{index=0}
        .arg("-vsync")
        .arg("0")                                    // no duplicate / drop – pass through
        .rawvideo()                                  // -f rawvideo -pix_fmt rgb24 -  :contentReference[oaicite:1]{index=1}
        .spawn()?;

    // Iterate over decoded frames -------------------------------
    thread::spawn(move || {
        // The iterator yields log events *and* frames – the
        // extension trait gives us a convenient `.filter_frames()`.
        if let Ok(iter) = child.iter() {
            for frame in iter.filter_frames() {      //  :contentReference[oaicite:2]{index=2}
                let rgb_image = ImageRgb888 {
                    header: Some(Header {
                        timestamp: Timestamp::get_current_time().into(),
                        reference_id: 0,
                        entity_path: "/camera".to_string(),
                    }),
                    width: frame.width,
                    height: frame.height,
                    data: frame.data, // Assuming the data is already in RGBA format
                };

                // Atomically replace the previous frame.
                let mut slot = latest.lock().unwrap();
                *slot = Some(rgb_image);

                println!("Received frame: {}x{}", frame.width, frame.height);
            }
        }
    });

    Ok(())
}

fn take_latest(latest: &LatestFrame) -> Option<ImageRgb888> {
    latest.lock().unwrap().take()
}

fn main() -> Result<()> {
    make87::initialize();

    let rtsp_url = "rtsp://your_rtsp_stream_url"; // Replace with your RTSP URL

    // Shared frame slot
    let latest: LatestFrame = Arc::new(Mutex::new(None));

    // Kick off background reader
    spawn_ffmpeg_reader(rtsp_url, Arc::clone(&latest))?;

    let topic_name = "OUTGOING_FRAME";
    let publisher = make87::resolve_topic_name(topic_name)
        .and_then(|resolved| make87::get_publisher::<ImageRgb888>(resolved))
        .expect("Failed to resolve or create publisher");

    // Poll & publish
    loop {
        if let Some(image) = take_latest(&latest) {
            match publisher.publish(&image) {
                Ok(()) => println!("Published frame: {}x{}", image.width, image.height),
                Err(_) => eprintln!("Failed to publish frame"),
            }
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
}


