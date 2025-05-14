use anyhow::Result;
use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::FfmpegEvent;
use futures::stream::{StreamExt, SelectAll};
use make87_messages::core::Header;
use make87_messages::google::protobuf::Timestamp;
use make87_messages::image::uncompressed::ImageRgb888;
use tokio::sync::watch;
use tokio::task;
use tokio_stream::wrappers::WatchStream;
use url::Url;

type FrameSender = watch::Sender<Option<ImageRgb888>>;
type FrameReceiver = watch::Receiver<Option<ImageRgb888>>;

struct CameraConfig {
    ip: Vec<String>,
    port: Vec<u32>,
    uri_suffix: Vec<String>,
    username: Vec<String>,
    password: Vec<String>,
    stream_index: Vec<u32>,
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
async fn spawn_ffmpeg_reader(rtsp_url: String, stream_index: u32, sender: FrameSender, camera_idx: usize) -> Result<()> {
    let entity_path = format_entity_path(&rtsp_url);

    task::spawn_blocking(move || {
        let mut child = FfmpegCommand::new()
            .args([
                "-rtsp_transport", "tcp",
                "-timeout", "5000000",        // 5 s read‑timeout
                "-allowed_media_types", "video",
            ])
            .input(&rtsp_url)
            .fps_mode("passthrough")
            .rawvideo()
            .spawn()
            .expect(&format!("Failed to spawn ffmpeg (camera {camera_idx})"));

        if let Ok(iter) = child.iter() {
            for event in iter {
                match event {
                    FfmpegEvent::ParsedVersion(v) => {
                        println!("[camera {camera_idx}] Parsed FFmpeg version: {:?}", v);
                    }
                    FfmpegEvent::ParsedConfiguration(c) => {
                        println!("[camera {camera_idx}] Parsed FFmpeg configuration: {:?}", c);
                    }
                    FfmpegEvent::ParsedStreamMapping(mapping) => {
                        println!("[camera {camera_idx}] Parsed stream mapping: {}", mapping);
                    }
                    FfmpegEvent::ParsedInput(input) => {
                        println!("[camera {camera_idx}] Parsed input: {:?}", input);
                    }
                    FfmpegEvent::ParsedOutput(output) => {
                        println!("[camera {camera_idx}] Parsed output: {:?}", output);
                    }
                    FfmpegEvent::ParsedInputStream(stream) => {
                        println!("[camera {camera_idx}] Parsed input stream: {:?}", stream);
                    }
                    FfmpegEvent::ParsedOutputStream(stream) => {
                        println!("[camera {camera_idx}] Parsed output stream: {:?}", stream);
                    }
                    FfmpegEvent::ParsedDuration(duration) => {
                        println!("[camera {camera_idx}] Parsed duration: {:?}", duration);
                    }
                    FfmpegEvent::Log(level, msg) => {
                        println!("[camera {camera_idx}] FFmpeg log [{:?}]: {}", level, msg);
                    }
                    FfmpegEvent::LogEOF => {
                        println!("[camera {camera_idx}] FFmpeg log ended");
                    }
                    FfmpegEvent::Error(err) => {
                        eprintln!("[camera {camera_idx}] Error: {}", err);
                    }
                    FfmpegEvent::Progress(progress) => {
                        println!("[camera {camera_idx}] Progress: {:?}", progress);
                    }
                    FfmpegEvent::OutputFrame(frame) => {
                        if frame.output_index != stream_index {
                            continue;
                        }

                        println!(
                            "[camera {camera_idx}] Received output frame: {}x{}, fmt={}, index={}, ts={}",
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
                            eprintln!("[camera {camera_idx}] Channel closed, stopping reader thread");
                            break;
                        }
                    }
                    FfmpegEvent::OutputChunk(chunk) => {
                        println!("[camera {camera_idx}] Received output chunk ({} bytes)", chunk.len());
                    }
                    FfmpegEvent::Done => {
                        println!("[camera {camera_idx}] FFmpeg processing done");
                    }
                }
            }
        }
    });

    Ok(())
}

fn parse_csv<T: std::str::FromStr>(input: &str, field: &str) -> Result<Vec<T>, anyhow::Error>
where
    <T as std::str::FromStr>::Err: std::fmt::Display,
{
    input
        .split(',')
        .map(|s| s.trim().parse::<T>().map_err(|e| anyhow::anyhow!("Invalid value in {}: {}", field, e)))
        .collect()
}

fn load_camera_config() -> Result<CameraConfig, anyhow::Error> {
    let username = make87::get_config_value("CAMERA_USERNAME").unwrap_or_default();
    let password = make87::get_config_value("CAMERA_PASSWORD").unwrap_or_default();
    let ip = make87::get_config_value("CAMERA_IP")
        .ok_or_else(|| anyhow::anyhow!("CAMERA_IP is required"))?;
    let port = make87::get_config_value("CAMERA_PORT").unwrap_or_else(|| "554".to_string());
    let uri_suffix = make87::get_config_value("CAMERA_URI_SUFFIX").unwrap_or_default();
    let stream_index = make87::get_config_value("STREAM_INDEX").unwrap_or_else(|| "0".to_string());

    let usernames = parse_csv::<String>(&username, "CAMERA_USERNAME")?;
    let passwords = parse_csv::<String>(&password, "CAMERA_PASSWORD")?;
    let ips = parse_csv::<String>(&ip, "CAMERA_IP")?;
    let ports = parse_csv::<u32>(&port, "CAMERA_PORT")?;
    let uri_suffixes = parse_csv::<String>(&uri_suffix, "CAMERA_URI_SUFFIX")?;
    let stream_indices = parse_csv::<u32>(&stream_index, "STREAM_INDEX")?;

    let config_fields = [
        ("CAMERA_USERNAME", usernames.len()),
        ("CAMERA_PASSWORD", passwords.len()),
        ("CAMERA_IP", ips.len()),
        ("CAMERA_PORT", ports.len()),
        ("CAMERA_URI_SUFFIX", uri_suffixes.len()),
        ("STREAM_INDEX", stream_indices.len()),
    ];
    let expected = config_fields.iter().map(|(_, l)| *l).max().unwrap_or(1);

    if !config_fields.iter().all(|&(_, l)| l == expected) {
        let details = config_fields
            .iter()
            .map(|(name, len)| format!("{}: {}", name, len))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(anyhow::anyhow!(
            "All camera config fields must have the same number of comma-separated values.\nField lengths: {}\nExpected: {}",
            details,
            expected
        ));
    }

    Ok(CameraConfig {
        username: usernames,
        password: passwords,
        ip: ips,
        port: ports,
        uri_suffix: uri_suffixes,
        stream_index: stream_indices,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    make87::initialize();

    let config = load_camera_config()?;

    let publisher = make87::resolve_topic_name("CAMERA_RGB")
        .and_then(|resolved| make87::get_publisher::<ImageRgb888>(resolved))
        .expect("Failed to resolve or create publisher");

    let mut receivers = Vec::new();

    for idx in 0..config.ip.len() {
        // Compose RTSP URL with optional username/password
        let user = config.username.get(idx).and_then(|u| {
            if !u.is_empty() { Some(u) } else { None }
        });
        let pass = config.password.get(idx).and_then(|p| {
            if !p.is_empty() { Some(p) } else { None }
        });

        let userinfo = match (user, pass) {
            (Some(u), Some(p)) => format!("{}:{}@", u, p),
            (Some(u), None) => format!("{}@", u),
            (None, _) => "".to_string(),
        };

        let rtsp_url = format!(
            "rtsp://{}{host}:{port}/{path}",
            userinfo,
            host = config.ip[idx],
            port = config.port[idx],
            path = config.uri_suffix[idx]
        );
        
        let (sender, receiver) = watch::channel(None);
        receivers.push(receiver);
        // Spawn each ffmpeg reader, passing camera index
        let stream_index = config.stream_index[idx];
        tokio::spawn(spawn_ffmpeg_reader(rtsp_url.clone(), stream_index, sender, idx));
    }

    // Wrap all receivers as streams and select over them
    let mut select_all = SelectAll::new();
    for receiver in receivers {
        select_all.push(WatchStream::new(receiver));
    }

    // Publish frames as they arrive from any camera
    select_all
        .filter_map(|maybe_image| async move { maybe_image })
        .for_each(|image| {
            let publisher = &publisher;
            async move {
                if let Err(e) = publisher.publish_async(&image).await {
                    eprintln!("Failed to publish frame: {e}");
                }
            }
        })
        .await;

    Ok(())
}
