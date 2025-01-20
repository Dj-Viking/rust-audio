use std::{
	ffi::CString,
	fs::File,
	io::{BufReader, BufWriter, Read},
	os::unix::net::UnixStream,
	path::Path,
};

use qdft::QDFT;
use anyhow::{bail, Context};
use byteorder::ReadBytesExt;
use pulseaudio::protocol;
use num::complex::Complex;
use num::complex::ComplexFloat;

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
			name: Some(CString::new(&*device_name)?),
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

	let mut qdft = QDFT::<f32, f32>::new(
    	source_info.sample_spec.sample_rate as f64,
		(30.0, source_info.sample_spec.sample_rate as f64 / 2.0),
		48.0,
		0.0,
		Some((0.5,-0.5))
	);

	// buffer for the audio samples
	let mut buf = vec![0; record_stream.buffer_attr.fragment_size as usize];

	println!("frag size {}",record_stream.buffer_attr.fragment_size);

	let mut complex_vec = vec![Complex::<f32>::ZERO; qdft.size()];
	// read messages from the server in a loop. 
	// should pool socket here.....
	loop {
		let desc = protocol::read_descriptor(&mut sock)?;

		if desc.channel == u32::MAX {
			let (_, msg) = protocol::Command::read_tag_prefixed(
				&mut sock,
				protocol_version,
			)?;
			println!("command from server {:#?}", msg);
		} else {

			println!("got {} bytes of data", desc.length);
			buf.resize(desc.length as usize,0);

			// read the data
			sock.read_exact(&mut buf)?;
			//break;

			let mut cursor = std::io::Cursor::new(buf.as_slice());
			while cursor.position() < cursor.get_ref().len() as u64 {
				match record_stream.sample_spec.format {
					protocol::SampleFormat::S32Le => {
						let sample = cursor.read_i32::<byteorder::LittleEndian>()?;
						qdft.qdft_scalar(&(sample as f32), &mut complex_vec);
						println!("huh {:#?}", complex_vec.iter().map(|x| ComplexFloat::abs(*x)).collect::<Vec<_>>());
					},
					_ => unreachable!(),
				};
			}
		}

		// break;
	}

	Ok(())
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
