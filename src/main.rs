use clap::{app_from_crate, Arg};
use indicatif::{ProgressBar, ProgressStyle};
use postgres::types::ToSql;
use postgres::{Client, NoTls};

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Write;

#[derive(Default)]
struct Entry {
    /// Any state groups that has this state group as a prev_group
    next_state_groups: Vec<i64>,
    /// The state group that this one points to, if any
    prev_state_group: Option<i64>,
    /// Whether an event references this state group or not
    is_referenced: bool,
}

/// Get state groups from the database. If `room_id` is set then its limited
/// to state groups for that room
fn get_from_db(db_url: &str, room_id: Option<&str>) -> BTreeMap<i64, Entry> {
    let mut conn = Client::connect(db_url, NoTls).unwrap();

    let mut sql = r#"
        SELECT
            main.id AS state_group,
            forwards.state_group AS next,
            backwards.prev_state_group AS prev,
            EXISTS (SELECT 1 FROM event_to_state_groups WHERE state_group = main.id) AS is_referenced
        FROM state_groups AS main
        LEFT JOIN state_group_edges AS backwards ON (main.id = backwards.state_group)
        LEFT JOIN state_group_edges AS forwards ON (main.id = forwards.prev_state_group)
    "#.to_string();
    let mut args: Vec<&(dyn ToSql + Sync)> = Vec::new();

    if let Some(room_id) = &room_id {
        sql.push_str(" WHERE room_id = $1");
        args.push(room_id);
    }

    let stmt = conn.prepare(&sql).unwrap();
    let mut trans = conn.transaction().unwrap();

    let portal = trans.bind(&stmt, &*args).unwrap();

    let mut state_group_map: BTreeMap<i64, Entry> = BTreeMap::new();

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner().template("{spinner} [{elapsed}] {pos} rows retrieved"),
    );
    pb.enable_steady_tick(100);

    let mut num_rows = 0;

    loop {
        let rows = trans.query_portal(&portal, 10000).unwrap();
        if rows.len() == 0 {
            break;
        }

        for row in rows {
            let state_group = row.get(0);

            // We might get multiple rows per state_group due to having multiple
            // next state groups.
            let entry = state_group_map.entry(state_group).or_default();

            if let Some(next_group) = row.get(1) {
                entry.next_state_groups.push(next_group);
            }

            // These will all remain the same though.
            entry.prev_state_group = row.get(2);
            entry.is_referenced = row.get(3);

            pb.inc(1);
            num_rows += 1
        }
    }

    pb.set_length(num_rows);
    pb.finish();

    state_group_map
}

/// Get any missing state groups from the database
fn get_missing_from_db(db_url: &str, missing_sgs: &[i64]) -> BTreeMap<i64, Entry> {
    let mut conn = Client::connect(db_url, NoTls).unwrap();

    let stmt = conn
        .prepare(
            r#"
            SELECT
                main.id AS state_group,
                forwards.state_group AS next,
                backwards.prev_state_group AS prev,
                EXISTS (SELECT 1 FROM event_to_state_groups WHERE state_group = main.id) AS is_referenced
            FROM (SELECT $1::bigint AS id) AS main
            LEFT JOIN state_group_edges AS backwards ON (main.id = backwards.state_group)
            LEFT JOIN state_group_edges AS forwards ON (main.id = forwards.prev_state_group)
        "#,
        ).unwrap();

    let mut state_group_map: BTreeMap<i64, Entry> = BTreeMap::new();

    for missing_sg in missing_sgs {
        let rows = conn.query(&stmt, &[&missing_sg]).unwrap();

        for row in &rows {
            let state_group = row.get(0);

            // We might get multiple rows per state_group due to having multiple
            // next state groups.
            let entry = state_group_map.entry(state_group).or_default();

            if let Some(next_group) = row.get(1) {
                entry.next_state_groups.push(next_group);
            }

            // These will all remain the same though.
            entry.prev_state_group = row.get(2);
            entry.is_referenced = row.get(3);
        }
    }

    state_group_map
}

fn main() {
    let matches = app_from_crate!()
        .arg(
            Arg::new("postgres-url")
                .short('p')
                .value_name("URL")
                .help("The url for connecting to the postgres database")
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::new("room_id")
                .short('r')
                .value_name("ROOM_ID")
                .help("The room to process")
                .takes_value(true),
        )
        .arg(
            Arg::new("output")
                .short('o')
                .value_name("FILE")
                .help("File to output unreferenced groups to")
                .takes_value(true),
        )
        .get_matches();

    let db_url = matches
        .value_of("postgres-url")
        .expect("db url should be required");

    let room_id = matches.value_of("room_id");

    let mut output_file = matches
        .value_of("output")
        .map(|path| File::create(path).unwrap());

    // Fetch the initial set of groups from the DB.
    let mut map = get_from_db(db_url, room_id);

    println!("Fetched {} state groups from DB", map.len());

    // Sometimes we'll be missing state groups that are referenced, so we
    // iteratively find and fetch and missing state groups. This should only
    // happen when a `room_id` has been specified.
    let mut added: BTreeSet<i64> = map.keys().cloned().collect();
    let mut missing = Vec::new();
    loop {
        missing.clear();

        for sg in &added {
            for next_sg in &map[sg].next_state_groups {
                if !map.contains_key(next_sg) {
                    missing.push(*next_sg);
                }
            }

            if let Some(prev_sg) = map[sg].prev_state_group {
                if !map.contains_key(&prev_sg) {
                    missing.push(prev_sg);
                }
            }
        }

        if missing.is_empty() {
            break;
        }

        missing.sort_unstable();
        missing.dedup();

        println!("Fetching {} missing state groups from DB", missing.len());

        let updated = get_missing_from_db(db_url, &missing);

        println!("Got {} from DB", updated.len());

        added.clear();
        added.extend(updated.keys());

        let missing_set: BTreeSet<i64> = missing.iter().cloned().collect();

        let still_missing = missing_set.difference(&added).count();
        if still_missing > 0 {
            println!("Failed to find {} groups", still_missing);
        }

        map.extend(updated.into_iter());
    }

    println!("Total state groups: {}", map.len());

    // Now we propagate referenced flag, i.e. if a state group is referenced
    // then its prev group should also be marked as referenced, recursively.
    for state_group in map.keys().cloned().collect::<Vec<_>>() {
        let mut next = {
            let entry = &map[&state_group];
            if !entry.is_referenced {
                continue;
            }

            entry.prev_state_group
        };

        while let Some(sg) = next.take() {
            let entry = map.get_mut(&sg).unwrap();
            if !entry.is_referenced {
                entry.is_referenced = true;
                next = entry.prev_state_group;
            }
        }
    }

    let mut total = 0;
    for (state_group, entry) in &map {
        if !entry.is_referenced {
            total += 1;

            if let Some(ref mut fs) = output_file {
                writeln!(fs, "{}", state_group).unwrap();
            }
        }
    }

    println!("Found {} unreferenced groups", total);
}
