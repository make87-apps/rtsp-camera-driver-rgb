use anyhow::Result;
use ffmpeg_sidecar::command::FfmpegCommand;
use make87_messages::core::Header;
use make87_messages::google::protobuf::Timestamp;
use make87_messages::image::uncompressed::ImageRgb888;
use tokio::sync::watch;
use tokio::task;
use url::Url;

type FrameSender = watch::Sender<Option<ImageRgb888>>;
type FrameReceiver = watch::Receiver<Option<ImageRgb888>>;

/// Parses the RTSP URL into `/camera/<ip>/<path>`
fn format_entity_path(rtsp_url: &str) -> String {
    if let Ok(parsed) = Url::parse(rtsp_url) {
        let ip = parsed.host_str().unwrap_or("unknown");
        let path = parsed.path().trim_start_matches('/'); // remove leading slash
        format!("/camera/{}/{}", ip, path)
    } else {
        "/camera/unknown".to_string()
    }
}

/// Spawns a blocking thread to run FFmpeg and decode RGB888 frames.
async fn spawn_ffmpeg_reader(rtsp_url: &str, sender: FrameSender) -> Result<()> {
    let rtsp_url = rtsp_url.to_owned();
    let entity_path = format_entity_path(&rtsp_url);

    task::spawn_blocking(move || {
        let mut child = FfmpegCommand::new()
            .args(["-rtsp_transport", "tcp"])
            .input(&rtsp_url)
            .arg("-vsync")
            .arg("0")
            .rawvideo()
            .spawn()
            .expect("Failed to spawn ffmpeg");

        if let Ok(iter) = child.iter() {
            for frame in iter.filter_frames() {
                let timestamp = Timestamp::get_current_time().into();


                let rgb_image = ImageRgb888 {
                    header: Some(Header {
                        timestamp: Some(timestamp),
                        reference_id: 0,
                        entity_path: entity_path.clone(),
                    }),
                    width: frame.width,
                    height: frame.height,
                    data: frame.data,
                };

                if sender.send(Some(rgb_image)).is_err() {
                    eprintln!("Channel closed, stopping reader thread");
                    break;
                }

                println!("Received frame: {}x{}", frame.width, frame.height);
            }
        }
    });

    Ok(())
}

/// Consumes new frames from the receiver and publishes them asynchronously.
async fn publish_frames(mut receiver: FrameReceiver, topic_name: &str) -> Result<()> {
    let publisher = make87::resolve_topic_name(topic_name)
        .and_then(|resolved| make87::get_publisher::<ImageRgb888>(resolved))
        .expect("Failed to resolve or create publisher");

    loop {
        receiver.changed().await?;
        if let Some(image) = receiver.borrow().clone() {
            if let Err(e) = publisher.publish_async(&image).await {
                eprintln!("Failed to publish frame: {e}");
            } else {
                println!("Published frame: {}x{}", image.width, image.height);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    make87::initialize();

    let rtsp_url = "rtsp://your_rtsp_stream_url";
    let topic_name = "CAMERA_RGB";

    let (sender, receiver) = watch::channel(None);

    spawn_ffmpeg_reader(rtsp_url, sender).await?;
    publish_frames(receiver, topic_name).await?;

    Ok(())
}
