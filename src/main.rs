#![allow(dead_code)]
#![allow(unreachable_code)]
use std::{
	ffi::CString,
	io::{BufReader, Read},
	os::unix::net::UnixStream,
};

use std::io::Write;
use anyhow::{Context};
use byteorder::ReadBytesExt;
use pulseaudio::protocol;
use spectrum_analyzer::windows::hann_window;
use spectrum_analyzer::{samples_fft_to_spectrum, FrequencyLimit};
use spectrum_analyzer::scaling::divide_by_N_sqrt;

fn main() -> anyhow::Result<()> {
	let (mut sock, protocol_version) =
		connect_and_init().context("failed to init client")?;

	// i found this name by running `pacmd list-sources` and just finding the
	// name of the thing I want to get audio samples from
	// as in whatever outboard input device is connected to the already running pulseaudio server
	let device_name = "alsa_input.usb-Yamaha_Corporation_Yamaha_AG06MK2-00.analog-stereo";
	
	protocol::write_command_message(
		sock.get_mut(),
		10,
		protocol::Command::GetSourceInfo(protocol::GetSourceInfo {
			name: Some(CString::new(device_name)?),
			..Default::default()
		}),
		protocol_version,
	)?;

	let (_, source_info) =
		protocol::read_reply_message::<protocol::SourceInfo>(
			&mut sock, protocol_version
		)?;

	println!("socket {:#?}", sock);


	// make recording stream on the server
	protocol::write_command_message(
		sock.get_mut(),
		99,
		protocol::Command::CreateRecordStream(
			protocol::RecordStreamParams {
				source_index: Some(source_info.index),
				sample_spec: protocol::SampleSpec {
					format: source_info.sample_spec.format,
					channels: source_info.channel_map.num_channels(),
					sample_rate: source_info.sample_spec.sample_rate,
				},
				channel_map: source_info.channel_map,
				cvolume: Some(protocol::ChannelVolume::norm(2)),
				..Default::default()
			}
		),
		protocol_version,
	)?;

	let (_, record_stream) =
		protocol::read_reply_message::<protocol::CreateRecordStreamReply>(
		&mut sock,
		protocol_version,
	)?;

	println!("record strim reply {:#?}", record_stream);


	// buffer for the audio samples
	let mut buf = vec![0; record_stream.buffer_attr.fragment_size as usize];
	let mut float_buf = Vec::<f32>::new();

	println!("frag size {}",record_stream.buffer_attr.fragment_size);

	// nice
	let mut fft_buf = [0.0; 69];

	// read messages from the server in a loop. 
	// should poll(?) socket here.....
	loop {
		let desc = protocol::read_descriptor(&mut sock)?;
		if desc.channel == u32::MAX {
			let (_, msg) = protocol::Command::read_tag_prefixed(
				&mut sock,
				protocol_version,
			)?;
			println!("command from server {:#?}", msg);
		} else {
			// dunno why it's sending 0 bytes but sometimes does
			// if desc.length == 0 { continue; }

			// println!("{} | got {} bytes of data", i, desc.length);
			buf.resize(desc.length as usize,0);

			float_buf.clear();

			// read the data
			// Note:!!! if pavucontrol isn't running
			// then this socket read will be really slow
			// almost 2 seconds of latency between reads
			//
			// read the length of the buffer...
			// TODO: handle reading only the amount I need to per 'frame'
			// instead of just reading and exhausting the buffer before it gets filled up again
			// don't know how fast this loop as at the moment but maybe 60 fps? worth checking
			let n = sock.read(&mut buf)?;

			let mut cursor = std::io::Cursor::new(&buf[..n]);
			while cursor.position() < cursor.get_ref().len() as u64 {
				match record_stream.sample_spec.format {
					protocol::SampleFormat::S32Le => {
						let sample = cursor.read_i32::<byteorder::LittleEndian>()?;
						float_buf.push(sample as f32);
					}
					_ => unreachable!(),
				}
			}

			if float_buf.len() < 256 { continue; }
			let hann_window = hann_window(&float_buf[0..256]);

			let fft = samples_fft_to_spectrum(
				&hann_window,
				source_info.sample_spec.sample_rate,
				FrequencyLimit::Range(50.0, 12000.0),
				Some(&divide_by_N_sqrt),
			).unwrap();
			
			let fr_mags: Vec<(f32, f32)> = fft.data().iter().map(|(fr, mag)| (fr.val(), mag.val())).collect();

			// // Apply smoothing
			
			// let fr_mags = exponential_moving_average(&fr_mags, 0.2);

			const FACTOR: f32 = 0.98;
			
			// TODO: keep the example of the 'smoothing' that i figured out....might be useful
			// for the visualization so that I can look at a wider range of frequencies to use
			// for changing the visuals

			fr_mags.iter().map(|(_, x)| x)
				.zip(fft_buf.iter_mut()).for_each(|(c, p)| 
					if *c > *p { *p = *c; } 
					else { *p *= FACTOR; });

			// sometimes after a while I think something gets bugged out and 
			// the audio server takes a shit and lags pretty badly not sure why
			// clear
			
			print!("\x1B[2J\x1B[1;1H");

			let mut stdout = std::io::stdout();
			let mut handle = stdout.lock();
			// print bars for the magnitude of the frequency at that frequency value
			
			for (f, m) in fr_mags.iter().map(|(f, _)| f).zip(fft_buf.iter()) {
				let mut val = (m * f).sqrt() / 3000.0;

				// poor man's noise suppression
				val -= 30.0;
				if val < 30.0 { val = 0.0; } else { val = val.floor(); }

				let _ = writeln!(handle, "{val} - {f:.2}Hz => {}", "|".repeat(val as usize));
			}

			let _ = stdout.flush();
		}
	}

	Ok(())
}

fn exponential_moving_average(data: &[(f32, f32)], alpha: f32) -> Vec<(f32, f32)> {
    let mut smoothed = Vec::with_capacity(data.len());
    let mut prev = data[0];
    for &(fr, val) in data {
        let smoothed_val = alpha * val + (1.0 - alpha) * prev.1;
        smoothed.push((fr, smoothed_val));
        prev = (fr, smoothed_val);
    }
    smoothed
}

// establish an audio client for the pulseaudio server
fn connect_and_init() -> anyhow::Result<(BufReader<UnixStream>, u16)> {

    let socket_path = pulseaudio::socket_path_from_env().context("PulseAudio not available")?;
    let mut sock = std::io::BufReader::new(UnixStream::connect(socket_path)?);

    let cookie = pulseaudio::cookie_path_from_env()
        .and_then(|path| std::fs::read(path).ok())
        .unwrap_or_default();
    let auth = protocol::AuthParams {
        version: protocol::MAX_VERSION,
        supports_shm: false,
        supports_memfd: false,
        cookie,
    };

    protocol::write_command_message(
        sock.get_mut(),
        0,
        protocol::Command::Auth(auth),
        protocol::MAX_VERSION,
    )?;

    let (_, auth_reply) =
        protocol::read_reply_message::<protocol::AuthReply>(&mut sock, protocol::MAX_VERSION)?;
    let protocol_version = std::cmp::min(protocol::MAX_VERSION, auth_reply.version);

    let mut props = protocol::Props::new();
    props.set(
        protocol::Prop::ApplicationName,
        CString::new("pulseaudio-rs-playback").unwrap(),
    );
    protocol::write_command_message(
        sock.get_mut(),
        1,
        protocol::Command::SetClientName(props),
        protocol_version,
    )?;

    let _ =
        protocol::read_reply_message::<protocol::SetClientNameReply>(&mut sock, protocol_version)?;
    Ok((sock, protocol_version))
}
/* 
*sample_rate = 44100
channels = 2
bytes_per_sample = 4 //sl32le
bytes_per_second = sample_rate * channels * bytes_per_sample

start_time = get_monotonic_time();
frame_time = 1.0 / sample_rate

loop:
  current_time = get_monotonic_time()
  elapsed_time = current_time - start_time

  expected_bytes = elapsed_time * bytes_per_second
  available_bytes = read_from_stream()

  if available_bytes < expected_bytes :
    sleep(frame_time)
    continue

  process_audio()

  drift = get_monotonic_time() - (start_time + elapsed_time)
  if drift > ALLOWED_DRIFT:
    adjust_reading_rate() // to compensate for bad hardware
* */
