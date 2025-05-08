use anyhow::Result;
use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::FfmpegEvent;
use make87_messages::core::Header;
use make87_messages::google::protobuf::Timestamp;
use make87_messages::image::uncompressed::ImageRgb888;
use tokio::sync::watch;
use tokio::task;
use url::Url;

type FrameSender = watch::Sender<Option<ImageRgb888>>;
type FrameReceiver = watch::Receiver<Option<ImageRgb888>>;

struct CameraConfig {
    ip: String,
    port: u32,
    uri_suffix: String,
    username: String,
    password: String,
    stream_index: u32,
}

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
async fn spawn_ffmpeg_reader(rtsp_url: &str, stream_index: u32, sender: FrameSender) -> Result<()> {
    let rtsp_url = rtsp_url.to_owned();
    let entity_path = format_entity_path(&rtsp_url);

    task::spawn_blocking(move || {
        let mut child = FfmpegCommand::new()
            .args([
                "-rtsp_transport",
                "tcp",
            ])
            .input(&rtsp_url)
            .fps_mode("passthrough")
            .rawvideo()
            .spawn()
            .expect("Failed to spawn ffmpeg");

        if let Ok(iter) = child.iter() {
            for event in iter {
                match event {
                    FfmpegEvent::ParsedVersion(v) => {
                        println!("Parsed FFmpeg version: {:?}", v);
                    }
                    FfmpegEvent::ParsedConfiguration(c) => {
                        println!("Parsed FFmpeg configuration: {:?}", c);
                    }
                    FfmpegEvent::ParsedStreamMapping(mapping) => {
                        println!("Parsed stream mapping: {}", mapping);
                    }
                    FfmpegEvent::ParsedInput(input) => {
                        println!("Parsed input: {:?}", input);
                    }
                    FfmpegEvent::ParsedOutput(output) => {
                        println!("Parsed output: {:?}", output);
                    }
                    FfmpegEvent::ParsedInputStream(stream) => {
                        println!("Parsed input stream: {:?}", stream);
                    }
                    FfmpegEvent::ParsedOutputStream(stream) => {
                        println!("Parsed output stream: {:?}", stream);
                    }
                    FfmpegEvent::ParsedDuration(duration) => {
                        println!("Parsed duration: {:?}", duration);
                    }
                    FfmpegEvent::Log(level, msg) => {
                        println!("FFmpeg log [{:?}]: {}", level, msg);
                    }
                    FfmpegEvent::LogEOF => {
                        println!("FFmpeg log ended");
                    }
                    FfmpegEvent::Error(err) => {
                        eprintln!("Error: {}", err);
                    }
                    FfmpegEvent::Progress(progress) => {
                        println!("Progress: {:?}", progress);
                    }
                    FfmpegEvent::OutputFrame(frame) => {
                        if frame.output_index != stream_index {
                            continue;
                        }

                        println!(
                            "Received output frame: {}x{}, fmt={}, index={}, ts={}",
                            frame.width,
                            frame.height,
                            frame.pix_fmt,
                            frame.output_index,
                            frame.timestamp
                        );

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
                    }
                    FfmpegEvent::OutputChunk(chunk) => {
                        println!("Received output chunk ({} bytes)", chunk.len());
                    }
                    FfmpegEvent::Done => {
                        println!("FFmpeg processing done");
                    }
                }
            }



            // for frame in iter.filter_frames() {
            //     if frame.output_index != stream_index {
            //         continue;
            //     }
            //     let timestamp = Timestamp::get_current_time().into();
            //
            //     let rgb_image = ImageRgb888 {
            //         header: Some(Header {
            //             timestamp: Some(timestamp),
            //             reference_id: 0,
            //             entity_path: entity_path.clone(),
            //         }),
            //         width: frame.width,
            //         height: frame.height,
            //         data: frame.data,
            //     };
            //
            //     if sender.send(Some(rgb_image)).is_err() {
            //         eprintln!("Channel closed, stopping reader thread");
            //         break;
            //     }
            //
            //     println!(
            //         "Received frame: {}x{}@{}",
            //         frame.width, frame.height, frame.timestamp
            //     );
            // }
        }
    });

    Ok(())
}

/// Consumes new frames from the receiver and publishes them asynchronously.
async fn publish_frames(mut receiver: FrameReceiver) -> Result<()> {
    let publisher = make87::resolve_topic_name("CAMERA_RGB")
        .and_then(|resolved| make87::get_publisher::<ImageRgb888>(resolved))
        .expect("Failed to resolve or create publisher");

    loop {
        receiver.changed().await?;
        if let Some(image) = receiver.borrow().clone() {
            if let Err(e) = publisher.publish_async(&image).await {
                eprintln!("Failed to publish frame: {e}");
            }
        }
    }
}

fn load_camera_config() -> Result<CameraConfig, anyhow::Error> {
    let username = make87::get_config_value("CAMERA_USERNAME")
        .ok_or_else(|| anyhow::anyhow!("CAMERA_USERNAME is required"))?;
    let password = make87::get_config_value("CAMERA_PASSWORD")
        .ok_or_else(|| anyhow::anyhow!("CAMERA_PASSWORD is required"))?;
    let ip = make87::get_config_value("CAMERA_IP")
        .ok_or_else(|| anyhow::anyhow!("CAMERA_IP is required"))?;

    let port = make87::get_config_value("CAMERA_PORT").unwrap_or_else(|| "554".to_string()).parse::<u32>().map_err(|e| anyhow::anyhow!("STREAM_INDEX must be a valid u32: {}", e))?;
    let uri_suffix = make87::get_config_value("CAMERA_URI_SUFFIX").unwrap_or_default();
    let stream_index = make87::get_config_value("STREAM_INDEX")
        .unwrap_or_else(|| "0".to_string())
        .parse::<u32>()
        .map_err(|e| anyhow::anyhow!("STREAM_INDEX must be a valid u32: {}", e))?;

    Ok(CameraConfig {
        username,
        password,
        ip,
        port,
        uri_suffix,
        stream_index,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    make87::initialize();


    let config = load_camera_config()?;


    let rtsp_url = format!(
        "rtsp://{}:{}@{}:{}/{}",
        config.username,
        config.password,
        config.ip,
        config.port,
        config.uri_suffix
    );

    let (sender, receiver) = watch::channel(None);

    spawn_ffmpeg_reader(&rtsp_url, config.stream_index, sender).await?;
    publish_frames(receiver).await?;

    Ok(())
}
