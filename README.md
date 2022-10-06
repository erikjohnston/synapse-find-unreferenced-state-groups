# Find unreferenced state groups

Due to "reasons" synapse will sometimes persist state groups that don't later
then get referenced by events (or other state groups that are referenced by
events). This tool scans the table (optionally limiting to a room) and finds
such unreferenced groups.

**CAUTION**: While synapse is running and processing events there is a delay
between the state groups and the events being persisted. As such this tool may
pick up recent state groups that *will in future* get referenced by events that
are still getting processed. If you ran `rust-synapse-find-unreferenced-state-groups`,
while Synapse was running, do *not* blindly delete all the state groups that
were returned.

If you stop Synapse before running `rust-synapse-find-unreferenced-state-groups`,
it is safe to delete all the state groups that are returned.

## Usage

```bash
$ rust-synapse-find-unreferenced-state-groups --help
rust-synapse-find-unreferenced-state-groups 0.1.0
Erik Johnston


USAGE:
    rust-synapse-find-unreferenced-state-groups [OPTIONS] -p <URL>

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information

OPTIONS:
    -o <FILE>           File to output unreferenced groups to
    -p <URL>            The url for connecting to the postgres database
    -r <ROOM_ID>        The room to process
```

For example:
```bash
rust-synapse-find-unreferenced-state-groups -p postgres://user:pass@localhost/synapse -r '!cURbafjkfsMDVwdRDQ:matrix.org' -o /tmp/sgs.txt
```
To delete the unreferenced state groups, use something like this in postgres:

```sql
CREATE TEMPORARY TABLE unreffed(id BIGINT PRIMARY KEY);
COPY unreffed FROM '/tmp/sgs.txt' WITH (FORMAT 'csv');
DELETE FROM state_groups_state WHERE state_group IN (SELECT id FROM unreffed);
DELETE FROM state_group_edges WHERE state_group IN (SELECT id FROM unreffed);
DELETE FROM state_groups WHERE id IN (SELECT id FROM unreffed);
```
