use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::Read,
    path::Path,
};

use flate2::read::GzDecoder;
use regex::Regex;

use super::{
    script::parse_game_script,
    types::{
        BuildCommand, CommandOptionFlags, CommandRecord, DecodedCommand, DecodedTarget,
        InsertedCommand, RemovedCommand, ReplayDemInfo,
    },
};

pub(super) fn read_dem_info(path: &Path) -> Result<ReplayDemInfo, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    let replay = fs::File::open(path)?;
    let mut decoder = GzDecoder::new(replay);
    decoder.read_to_end(&mut bytes)?;

    let engine_version = detect_engine_version(&bytes)?;
    let script = extract_game_script(&bytes)?;
    let springie_stats = extract_springie_stats(&bytes);
    let command_history = extract_command_history(&bytes)?;

    Ok(ReplayDemInfo {
        engine_version,
        script,
        springie_stats,
        command_history,
    })
}

fn extract_command_history(bytes: &[u8]) -> Result<Vec<CommandRecord>, Box<dyn std::error::Error>> {
    let demo_header = parse_demo_header(bytes)?;
    let stream_start = demo_header.header_size + demo_header.script_size;
    let stream_end = if demo_header.demo_stream_size == 0 {
        bytes.len()
    } else {
        stream_start
            .checked_add(demo_header.demo_stream_size)
            .ok_or("demo stream size overflow")?
            .min(bytes.len())
    };

    if stream_start > bytes.len() || stream_start > stream_end {
        return Err("invalid demo stream offsets".into());
    }

    let mut offset = stream_start;
    let mut current_frame = 0u32;
    let mut player_selection: HashMap<u8, Vec<i32>> = HashMap::new();
    let mut commands = Vec::new();

    while offset + 8 <= stream_end {
        let mod_game_time = f32::from_le_bytes(bytes[offset..offset + 4].try_into()?);
        let chunk_len = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into()?) as usize;
        offset += 8;

        if offset + chunk_len > stream_end {
            break;
        }

        let packet = &bytes[offset..offset + chunk_len];
        offset += chunk_len;
        if packet.is_empty() {
            continue;
        }

        match packet[0] {
            1 => {
                if packet.len() >= 5 {
                    current_frame = i32::from_le_bytes(packet[1..5].try_into()?).max(0) as u32;
                }
            }
            2 => {
                current_frame = current_frame.saturating_add(1);
            }
            11 => {
                if let Some(record) =
                    parse_command_packet(packet, current_frame, mod_game_time, &player_selection)?
                {
                    commands.push(record);
                }
            }
            14 => {
                if let Some(record) = parse_ai_command_packet(packet, current_frame, mod_game_time)?
                {
                    commands.push(record);
                }
            }
            15 => {
                commands.extend(parse_ai_commands_packet(
                    packet,
                    current_frame,
                    mod_game_time,
                )?);
            }
            76 => {
                if let Some(record) =
                    parse_ai_command_tracked_packet(packet, current_frame, mod_game_time)?
                {
                    commands.push(record);
                }
            }
            12 => {
                parse_select_packet(packet, &mut player_selection)?;
            }
            _ => {}
        }
    }

    Ok(commands)
}

#[derive(Debug)]
struct DemoHeader {
    header_size: usize,
    script_size: usize,
    demo_stream_size: usize,
}

fn parse_demo_header(bytes: &[u8]) -> Result<DemoHeader, Box<dyn std::error::Error>> {
    if bytes.len() < 352 {
        return Err("demo file too short for header".into());
    }
    if &bytes[..15] != b"spring demofile" {
        return Err("invalid demo header magic".into());
    }

    let header_size = u32::from_le_bytes(bytes[20..24].try_into()?) as usize;
    let script_size = i32::from_le_bytes(bytes[304..308].try_into()?).max(0) as usize;
    let demo_stream_size = i32::from_le_bytes(bytes[308..312].try_into()?).max(0) as usize;

    Ok(DemoHeader {
        header_size,
        script_size,
        demo_stream_size,
    })
}

fn parse_command_packet(
    packet: &[u8],
    current_frame: u32,
    mod_game_time: f32,
    player_selection: &HashMap<u8, Vec<i32>>,
) -> Result<Option<CommandRecord>, Box<dyn std::error::Error>> {
    if packet.len() < 9 {
        return Ok(None);
    }
    let message_size = u16::from_le_bytes(packet[1..3].try_into()?) as usize;
    let packet = &packet[..packet.len().min(message_size.max(9))];
    if packet.len() < 9 {
        return Ok(None);
    }

    let player_id = packet[3];
    let command_id = i32::from_le_bytes(packet[4..8].try_into()?);
    let options = packet[8] as u32;
    let mut params = Vec::new();
    let mut pos = 9;
    while pos + 4 <= packet.len() {
        params.push(f32::from_le_bytes(packet[pos..pos + 4].try_into()?));
        pos += 4;
    }

    let decoded = decode_command(command_id, options, &params);

    Ok(Some(CommandRecord {
        frame: current_frame,
        game_seconds: safe_game_seconds(mod_game_time, current_frame),
        player_id: player_id as u32,
        ai_id: None,
        command_id,
        options,
        params,
        selected_unit_ids: player_selection
            .get(&player_id)
            .cloned()
            .unwrap_or_default(),
        decoded,
    }))
}

fn parse_ai_command_packet(
    packet: &[u8],
    current_frame: u32,
    mod_game_time: f32,
) -> Result<Option<CommandRecord>, Box<dyn std::error::Error>> {
    if packet.len() < 12 {
        return Ok(None);
    }
    let message_size = u16::from_le_bytes(packet[1..3].try_into()?) as usize;
    let packet = &packet[..packet.len().min(message_size.max(12))];
    if packet.len() < 12 {
        return Ok(None);
    }

    let player_id = packet[3];
    let ai_id = packet[4];
    let unit_id = i16::from_le_bytes(packet[5..7].try_into()?) as i32;
    let command_id = i32::from_le_bytes(packet[7..11].try_into()?);
    let options = packet[11] as u32;
    let mut params = Vec::new();
    let mut pos = 12;
    while pos + 4 <= packet.len() {
        params.push(f32::from_le_bytes(packet[pos..pos + 4].try_into()?));
        pos += 4;
    }

    let decoded = decode_command(command_id, options, &params);
    Ok(Some(CommandRecord {
        frame: current_frame,
        game_seconds: safe_game_seconds(mod_game_time, current_frame),
        player_id: player_id as u32,
        ai_id: Some(ai_id as u32),
        command_id,
        options,
        params,
        selected_unit_ids: vec![unit_id],
        decoded,
    }))
}

fn parse_ai_command_tracked_packet(
    packet: &[u8],
    current_frame: u32,
    mod_game_time: f32,
) -> Result<Option<CommandRecord>, Box<dyn std::error::Error>> {
    if packet.len() < 16 {
        return Ok(None);
    }
    let message_size = u16::from_le_bytes(packet[1..3].try_into()?) as usize;
    let packet = &packet[..packet.len().min(message_size.max(16))];
    if packet.len() < 16 {
        return Ok(None);
    }

    let player_id = packet[3];
    let ai_id = packet[4];
    let unit_id = i16::from_le_bytes(packet[5..7].try_into()?) as i32;
    let command_id = i32::from_le_bytes(packet[7..11].try_into()?);
    let options = packet[11] as u32;
    let param_size = u16::from_le_bytes(packet[14..16].try_into()?) as usize;
    let mut params = Vec::new();
    let mut pos = 16;
    while pos + 4 <= packet.len() && pos + 4 <= 16 + param_size {
        params.push(f32::from_le_bytes(packet[pos..pos + 4].try_into()?));
        pos += 4;
    }

    let decoded = decode_command(command_id, options, &params);
    Ok(Some(CommandRecord {
        frame: current_frame,
        game_seconds: safe_game_seconds(mod_game_time, current_frame),
        player_id: player_id as u32,
        ai_id: Some(ai_id as u32),
        command_id,
        options,
        params,
        selected_unit_ids: vec![unit_id],
        decoded,
    }))
}

fn parse_ai_commands_packet(
    packet: &[u8],
    current_frame: u32,
    mod_game_time: f32,
) -> Result<Vec<CommandRecord>, Box<dyn std::error::Error>> {
    if packet.len() < 16 {
        return Ok(Vec::new());
    }
    let message_size = u16::from_le_bytes(packet[1..3].try_into()?) as usize;
    let packet = &packet[..packet.len().min(message_size.max(16))];
    if packet.len() < 16 {
        return Ok(Vec::new());
    }

    let player_id = packet[3];
    let ai_id = packet[4];
    let mut pos = 11;
    let same_command_id = i32::from_le_bytes(packet[7..11].try_into()?);
    let same_options = packet[11] as u32;
    pos += 1;
    let same_param_size = u16::from_le_bytes(packet[pos..pos + 2].try_into()?) as usize;
    pos += 2;

    if pos + 2 > packet.len() {
        return Ok(Vec::new());
    }
    let unit_count = i16::from_le_bytes(packet[pos..pos + 2].try_into()?).max(0) as usize;
    pos += 2;
    let mut unit_ids = Vec::with_capacity(unit_count);
    for _ in 0..unit_count {
        if pos + 2 > packet.len() {
            return Ok(Vec::new());
        }
        unit_ids.push(i16::from_le_bytes(packet[pos..pos + 2].try_into()?) as i32);
        pos += 2;
    }

    if pos + 2 > packet.len() {
        return Ok(Vec::new());
    }
    let command_count = i16::from_le_bytes(packet[pos..pos + 2].try_into()?).max(0) as usize;
    pos += 2;

    let mut records = Vec::with_capacity(command_count);
    for _ in 0..command_count {
        if pos + 5 > packet.len() {
            break;
        }
        let command_id = same_command_id;
        let options = same_options;
        let param_bytes = same_param_size;
        if pos + param_bytes > packet.len() {
            break;
        }

        let mut params = Vec::new();
        let mut param_pos = pos;
        while param_pos + 4 <= pos + param_bytes {
            params.push(f32::from_le_bytes(
                packet[param_pos..param_pos + 4].try_into()?,
            ));
            param_pos += 4;
        }
        pos += param_bytes;

        let decoded = decode_command(command_id, options, &params);
        records.push(CommandRecord {
            frame: current_frame,
            game_seconds: safe_game_seconds(mod_game_time, current_frame),
            player_id: player_id as u32,
            ai_id: Some(ai_id as u32),
            command_id,
            options,
            params,
            selected_unit_ids: unit_ids.clone(),
            decoded,
        });
    }

    Ok(records)
}

fn safe_game_seconds(mod_game_time: f32, current_frame: u32) -> f32 {
    if mod_game_time.is_finite() && mod_game_time >= 0.0 {
        mod_game_time
    } else {
        current_frame as f32 / 30.0
    }
}

fn parse_select_packet(
    packet: &[u8],
    player_selection: &mut HashMap<u8, Vec<i32>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if packet.len() < 4 {
        return Ok(());
    }
    let message_size = u16::from_le_bytes(packet[1..3].try_into()?) as usize;
    let packet = &packet[..packet.len().min(message_size.max(4))];
    if packet.len() < 4 {
        return Ok(());
    }

    let player_id = packet[3];
    let mut selected = Vec::new();
    let mut pos = 4;
    while pos + 2 <= packet.len() {
        selected.push(i16::from_le_bytes(packet[pos..pos + 2].try_into()?) as i32);
        pos += 2;
    }
    player_selection.insert(player_id, selected);
    Ok(())
}

fn decode_command(command_id: i32, options: u32, params: &[f32]) -> DecodedCommand {
    let option_flags = decode_command_options(options);

    if command_id < 0 {
        return DecodedCommand {
            kind: "build".to_string(),
            option_flags,
            target: None,
            state: None,
            build: Some(BuildCommand {
                unit_def_id: command_id.unsigned_abs(),
                unit_name: None,
                x: params.first().copied(),
                y: params.get(1).copied(),
                z: params.get(2).copied(),
                facing: params.get(3).map(|value| *value as i32),
            }),
            inserted: None,
            removed: None,
        };
    }

    let (kind, target, state, inserted, removed) = match command_id {
        0 => ("stop".to_string(), None, None, None, None),
        1 => {
            let inserted = if params.len() >= 3 {
                let nested_command_id = params[1] as i32;
                let nested_options = params[2].max(0.0) as u32;
                let nested_params = params[3..].to_vec();
                Some(InsertedCommand {
                    index: params[0] as i32,
                    command_id: nested_command_id,
                    options: nested_options,
                    params: nested_params.clone(),
                    decoded: Box::new(decode_command(
                        nested_command_id,
                        nested_options,
                        &nested_params,
                    )),
                })
            } else {
                None
            };
            ("insert".to_string(), None, None, inserted, None)
        }
        2 => {
            let mode = if option_flags.alt {
                "remove_by_command_id"
            } else if option_flags.ctrl {
                "remove_by_tag"
            } else {
                "remove"
            };
            (
                "remove".to_string(),
                None,
                None,
                None,
                Some(RemovedCommand {
                    params: params.to_vec(),
                    mode: mode.to_string(),
                }),
            )
        }
        5 => (
            "wait".to_string(),
            None,
            params.first().map(|v| *v as i32),
            None,
            None,
        ),
        6 => (
            "timewait".to_string(),
            None,
            params.first().map(|v| *v as i32),
            None,
            None,
        ),
        7 => ("deathwait".to_string(), None, None, None, None),
        8 => ("squadwait".to_string(), None, None, None, None),
        9 => ("gatherwait".to_string(), None, None, None, None),
        10 => ("move".to_string(), decode_target(params), None, None, None),
        15 => (
            "patrol".to_string(),
            decode_target(params),
            None,
            None,
            None,
        ),
        16 => ("fight".to_string(), decode_target(params), None, None, None),
        20 => (
            "attack".to_string(),
            decode_target(params),
            None,
            None,
            None,
        ),
        21 => (
            "area_attack".to_string(),
            decode_target(params),
            None,
            None,
            None,
        ),
        25 => ("guard".to_string(), decode_target(params), None, None, None),
        32 => (
            "resurrect".to_string(),
            decode_target(params),
            None,
            None,
            None,
        ),
        40 => (
            "repair".to_string(),
            decode_target(params),
            None,
            None,
            None,
        ),
        45 => (
            "fire_state".to_string(),
            None,
            params.first().map(|v| *v as i32),
            None,
            None,
        ),
        50 => (
            "move_state".to_string(),
            None,
            params.first().map(|v| *v as i32),
            None,
            None,
        ),
        55 => (
            "set_base".to_string(),
            decode_target(params),
            None,
            None,
            None,
        ),
        65 => ("self_destruct".to_string(), None, None, None, None),
        70 => ("set_wanted_max_speed".to_string(), None, None, None, None),
        75 => (
            "load_units".to_string(),
            decode_target(params),
            None,
            None,
            None,
        ),
        76 => (
            "load_onto".to_string(),
            decode_target(params),
            None,
            None,
            None,
        ),
        81 => (
            "unload_units".to_string(),
            decode_target(params),
            None,
            None,
            None,
        ),
        85 => (
            "onoff".to_string(),
            None,
            params.first().map(|v| *v as i32),
            None,
            None,
        ),
        90 => (
            "reclaim".to_string(),
            decode_target(params),
            None,
            None,
            None,
        ),
        100 => (
            "cloak".to_string(),
            None,
            params.first().map(|v| *v as i32),
            None,
            None,
        ),
        115 => (
            "repeat".to_string(),
            None,
            params.first().map(|v| *v as i32),
            None,
            None,
        ),
        31109 => (
            "raw_move".to_string(),
            decode_target(params),
            None,
            None,
            None,
        ),
        31110 => (
            "raw_build".to_string(),
            decode_target(params),
            None,
            None,
            None,
        ),
        31200 => (
            "want_cloak".to_string(),
            None,
            params.first().map(|v| *v as i32),
            None,
            None,
        ),
        33410 => ("rearm".to_string(), decode_target(params), None, None, None),
        34220 => (
            "priority".to_string(),
            None,
            params.first().map(|v| *v as i32),
            None,
            None,
        ),
        34223 => (
            "retreat".to_string(),
            None,
            params.first().map(|v| *v as i32),
            None,
            None,
        ),
        34923 => ("morph".to_string(), None, None, None, None),
        34924 => ("morph_stop".to_string(), None, None, None, None),
        34925 => ("jump".to_string(), decode_target(params), None, None, None),
        _ => (
            format!("unknown_{}", command_id),
            decode_target(params),
            None,
            None,
            None,
        ),
    };

    DecodedCommand {
        kind,
        option_flags,
        target,
        state,
        build: None,
        inserted,
        removed,
    }
}

pub(super) fn enrich_command_history_with_unit_names(
    commands: &mut [CommandRecord],
    script: &str,
    zk_path: Option<&Path>,
    cache: &mut HashMap<String, HashMap<u32, String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(zk_path) = zk_path else {
        return Ok(());
    };
    let script_meta = parse_game_script(script)?;
    let Some(game_name) = script_meta.game_name else {
        return Ok(());
    };
    let Some(game_version) = game_name.strip_prefix("Zero-K v").map(str::to_string) else {
        return Ok(());
    };

    if !cache.contains_key(&game_version) {
        let names = load_unit_def_names_for_version(zk_path, &game_version)?;
        cache.insert(game_version.clone(), names);
    }

    let Some(unit_names) = cache.get(&game_version) else {
        return Ok(());
    };
    for command in commands {
        enrich_decoded_command_with_unit_names(&mut command.decoded, unit_names);
    }
    Ok(())
}

fn enrich_decoded_command_with_unit_names(
    decoded: &mut DecodedCommand,
    unit_names: &HashMap<u32, String>,
) {
    if let Some(build) = decoded.build.as_mut() {
        if build.unit_name.is_none() {
            build.unit_name = unit_names.get(&build.unit_def_id).cloned();
        }
    }
    if let Some(inserted) = decoded.inserted.as_mut() {
        enrich_decoded_command_with_unit_names(&mut inserted.decoded, unit_names);
    }
}

fn load_unit_def_names_for_version(
    zk_path: &Path,
    game_version: &str,
) -> Result<HashMap<u32, String>, Box<dyn std::error::Error>> {
    let archive_hash = resolve_game_archive_hash(zk_path, game_version)?;
    let archive_path = zk_path.join("games").join(format!("{archive_hash}.sdz"));
    if !archive_path.is_file() {
        return Ok(HashMap::new());
    }

    let bytes = fs::read(&archive_path)?;
    let reader = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader)?;
    let mut file = match archive.by_name("gamedata/unitdefs.lua") {
        Ok(file) => file,
        Err(_) => return Ok(HashMap::new()),
    };

    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let mut map = HashMap::new();
    let re = Regex::new(r#"UnitDefs\[(\d+)\]\.name\s*=\s*"([^"]+)""#)?;
    for cap in re.captures_iter(&contents) {
        if let (Some(id), Some(name)) = (cap.get(1), cap.get(2)) {
            if let Ok(id) = id.as_str().parse::<u32>() {
                map.insert(id, name.as_str().to_string());
            }
        }
    }
    Ok(map)
}

fn resolve_game_archive_hash(
    zk_path: &Path,
    game_version: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let versions_path = zk_path
        .join("rapid")
        .join("repos.springrts.com")
        .join("zk")
        .join("versions.gz");
    let file = fs::File::open(versions_path)?;
    let mut decoder = GzDecoder::new(file);
    let mut text = String::new();
    decoder.read_to_string(&mut text)?;

    for line in text.lines() {
        let mut parts = line.splitn(4, ',');
        let _tag = parts.next();
        let hash = parts.next();
        let _unused = parts.next();
        let display_name = parts.next();
        if let (Some(hash), Some(display_name)) = (hash, display_name) {
            if display_name.trim() == format!("Zero-K v{}", game_version) {
                return Ok(hash.trim().to_string());
            }
        }
    }

    Err(format!("could not resolve archive hash for Zero-K {}", game_version).into())
}

fn decode_command_options(options: u32) -> CommandOptionFlags {
    CommandOptionFlags {
        alt: options & 128 != 0,
        ctrl: options & 64 != 0,
        meta: options & 4 != 0,
        shift: options & 32 != 0,
        right: options & 16 != 0,
        internal: options & 8 != 0,
    }
}

fn decode_target(params: &[f32]) -> Option<DecodedTarget> {
    match params.len() {
        1 => Some(DecodedTarget::Unit {
            unit_id: params[0] as i32,
        }),
        3 => Some(DecodedTarget::Position {
            x: params[0],
            y: params[1],
            z: params[2],
        }),
        n if n >= 4 => Some(DecodedTarget::Area {
            x: params[0],
            y: params[1],
            z: params[2],
            radius: params[3],
        }),
        _ => None,
    }
}

fn detect_engine_version(bytes: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
    let prefix = String::from_utf8_lossy(&bytes[..bytes.len().min(256)]);
    let version_re = Regex::new(r"(20\d{2}\.\d{2}\.\d{2}|105\.[0-9A-Za-z.\-]+)")?;
    let version = version_re
        .captures(&prefix)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str().to_string())
        .ok_or("could not detect replay engine version from dem header")?;
    Ok(version)
}

fn extract_game_script(bytes: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
    let marker = b"[game]";
    let start = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .ok_or("could not find [game] script block in dem")?;

    let mut brace_depth = 0i32;
    let mut found_open = false;
    let mut end = None;
    for (offset, byte) in bytes[start..].iter().enumerate() {
        match byte {
            b'{' => {
                brace_depth += 1;
                found_open = true;
            }
            b'}' if found_open => {
                brace_depth -= 1;
                if brace_depth == 0 {
                    end = Some(start + offset + 1);
                    break;
                }
            }
            _ => {}
        }
    }

    let end = end.ok_or("could not determine end of [game] script block")?;
    Ok(String::from_utf8_lossy(&bytes[start..end]).to_string())
}

fn extract_springie_stats(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let Ok(stats_re) = Regex::new(r"SPRINGIE:(stats|award),[^\0\r\n]+") else {
        return Vec::new();
    };

    let mut seen = BTreeMap::<String, ()>::new();
    for captures in stats_re.find_iter(&text) {
        seen.insert(captures.as_str().to_string(), ());
    }
    seen.into_keys().collect()
}
