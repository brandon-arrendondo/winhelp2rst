//! Debug tool: dump raw LinkData1 / LinkData2 bytes for topic records.
//!
//! Usage: cargo run --example dump_record -- <file.hlp> [context_id_prefix]
//!
//! Prints each text record's LinkData1 as hex + structured decode,
//! and LinkData2 as text. This exists to validate the topic record
//! format understanding behind the opcode parser.

use std::env;
use std::process::ExitCode;

use winhelp::{
    context_hash, extract_records, flatten_topic_stream, read_topic_blocks, ContextMap,
    HlpContainer, PhraseTable, SystemInfo, RECORD_TYPE_TABLE, RECORD_TYPE_TEXT, RECORD_TYPE_TOPIC,
};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: dump_record <file.hlp> [topic_ctx_hash_hex]");
        return ExitCode::FAILURE;
    }

    let path = &args[1];
    let want_ctx_hash: Option<u32> = args.get(2).and_then(|s| {
        let t = s.trim_start_matches("0x").trim_start_matches("ctx_");
        u32::from_str_radix(t, 16).ok()
    });

    let container = HlpContainer::open(std::path::Path::new(path)).expect("open");
    let system = SystemInfo::from_bytes(&container.read_file("|SYSTEM").unwrap()).unwrap();

    let phrases_data = container.read_file("|Phrases").ok();
    let phr_index = container.read_file("|PhrIndex").ok();
    let phrases = match phrases_data {
        Some(pd) => {
            PhraseTable::from_bytes(&pd, phr_index.as_deref(), system.phrases_compressed()).unwrap()
        }
        None => PhraseTable::empty(),
    };

    let topic_data = container.read_file("|TOPIC").unwrap();
    let blocks = read_topic_blocks(
        &topic_data,
        system.topic_block_size(),
        system.uses_lz77(),
        &phrases,
    )
    .unwrap();
    let stream = flatten_topic_stream(&blocks, system.decompress_size());
    let before_31 = system.minor_version <= 16;
    let mut records = extract_records(&stream, before_31).unwrap();

    // Apply phrase expansion to LinkData2 (matches lib.rs logic).
    if !phrases.is_empty() {
        for record in records.iter_mut() {
            if record.data_len2 > record.link_data2.len() {
                record.link_data2 = phrases.expand(&record.link_data2).unwrap();
            }
        }
    }

    let context_map = ContextMap::from_bytes(&container.read_file("|CONTEXT").unwrap()).unwrap();
    // Sort context entries by TOPICOFFSET.
    let mut ctx_sorted: Vec<(u32, u32)> = context_map
        .entries()
        .map(|(hash, topicoff)| (topicoff, hash))
        .collect();
    ctx_sorted.sort_by_key(|&(t, _)| t);

    // Mirror lib.rs TOPICOFFSET matching exactly. First pass: figure out
    // each TOPIC record's index→hash mapping using the same running-offset
    // logic. Second pass: print records for the target topic.
    let mut topic_hash_by_idx: std::collections::HashMap<usize, u32> = Default::default();
    {
        let mut running: u32 = 0;
        let mut ctx_idx = 0usize;
        let mut last_topic_idx: Option<usize> = None;
        for (i, record) in records.iter().enumerate() {
            match record.record_type {
                RECORD_TYPE_TOPIC => {
                    while ctx_idx < ctx_sorted.len() && ctx_sorted[ctx_idx].0 <= running {
                        let (_, h) = ctx_sorted[ctx_idx];
                        topic_hash_by_idx.entry(i).or_insert(h);
                        ctx_idx += 1;
                    }
                    last_topic_idx = Some(i);
                }
                RECORD_TYPE_TEXT | RECORD_TYPE_TABLE => {
                    running = running.saturating_add(record.link_data2.len() as u32);
                    if let Some(ti) = last_topic_idx {
                        while ctx_idx < ctx_sorted.len() && ctx_sorted[ctx_idx].0 <= running {
                            let (_, h) = ctx_sorted[ctx_idx];
                            topic_hash_by_idx.entry(ti).or_insert(h);
                            ctx_idx += 1;
                        }
                    }
                }
                _ => {}
            }
            if !before_31 && (record.next_block as i32) > 0 && record.next_block >= 12 {
                let curr_block = record.stream_offset / 16384;
                let next_stream_off = record.next_block as usize - 12;
                let next_block_num = next_stream_off / 16384;
                if next_block_num != curr_block {
                    running = (next_block_num as u32) * 32768;
                }
            }
        }
    }

    // Group records into (topic_idx, records_for_this_topic) spans.
    let mut current_topic_hash: Option<u32> = None;
    let mut in_target = want_ctx_hash.is_none();
    let mut printed = 0usize;
    let mut topic_idx_of_last: Option<usize> = None;

    for (i, record) in records.iter().enumerate() {
        match record.record_type {
            RECORD_TYPE_TOPIC => {
                let my_hash = topic_hash_by_idx.get(&i).copied();
                topic_idx_of_last = Some(i);
                if want_ctx_hash.is_some() {
                    eprintln!(
                        "[TOPIC #{} @ stream_off {} hash={:?}]",
                        i,
                        record.stream_offset,
                        my_hash.map(|h| format!("0x{h:08x}")),
                    );
                }
                current_topic_hash = my_hash;
                if let Some(wanted) = want_ctx_hash {
                    in_target = my_hash == Some(wanted);
                }
                if in_target {
                    println!("\n============================================================");
                    println!(
                        "TOPIC header (hash={:?}, stream_off={})",
                        my_hash.map(|h| format!("0x{h:08x}")),
                        record.stream_offset
                    );
                    println!(
                        "LinkData1 ({} bytes) = {}",
                        record.link_data1.len(),
                        hex(&record.link_data1)
                    );
                    println!(
                        "LinkData2 ({} bytes) = {:?}",
                        record.link_data2.len(),
                        String::from_utf8_lossy(&record.link_data2)
                    );
                    printed += 1;
                }
            }
            RECORD_TYPE_TEXT | RECORD_TYPE_TABLE => {
                // Track the topic this belongs to (last seen TOPIC record).
                if let Some(ti) = topic_idx_of_last {
                    if let Some(h) = topic_hash_by_idx.get(&ti).copied() {
                        if let Some(wanted) = want_ctx_hash {
                            in_target = h == wanted;
                        }
                    }
                }
                if in_target {
                    println!(
                        "\n--- {} record #{} (hash={:?}, stream_off={}) ---",
                        if record.record_type == RECORD_TYPE_TEXT {
                            "TEXT"
                        } else {
                            "TABLE"
                        },
                        i,
                        current_topic_hash.map(|h| format!("0x{h:08x}")),
                        record.stream_offset
                    );
                    println!("LinkData1 ({} bytes):", record.link_data1.len());
                    println!("  hex:   {}", hex(&record.link_data1));
                    println!("  ascii: {}", printable(&record.link_data1));
                    println!("LinkData2 ({} bytes):", record.link_data2.len());
                    println!("  hex:   {}", hex(&record.link_data2));
                    println!("  ascii: {}", printable(&record.link_data2));
                    printed += 1;
                }
            }
            _ => {}
        }
        if want_ctx_hash.is_none() && printed > 40 {
            break;
        }
    }

    eprintln!("\ncontext entries: {}", ctx_sorted.len());
    eprintln!("e.g. 'abort' hash: 0x{:08x}", context_hash("abort"));

    ExitCode::SUCCESS
}

fn hex(b: &[u8]) -> String {
    let mut s = String::new();
    for (i, byte) in b.iter().enumerate() {
        if i > 0 && i % 16 == 0 {
            s.push_str("\n         ");
        } else if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{:02x}", byte));
    }
    s
}

fn printable(b: &[u8]) -> String {
    b.iter()
        .map(|&c| {
            if (0x20..0x7f).contains(&c) {
                c as char
            } else {
                '·'
            }
        })
        .collect()
}
