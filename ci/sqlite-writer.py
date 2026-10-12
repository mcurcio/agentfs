#!/usr/bin/env python3
"""A SQLite workload for ci/e2e-sqlite.sh.

Creates DB in the given journal mode with synchronous=NORMAL, then commits
TXS transactions of ROWS rows each, PAUSE seconds apart, from one
connection. After each commit it prints "committed <rows in table>" and
flushes, so the caller can follow along. After the last one it prints
"done <rows>" and keeps the connection (and, in WAL mode, the WAL) open
for IDLE seconds before closing.

    sqlite-writer.py DB --mode wal|delete [--txs N] [--rows N]
                        [--pause SECONDS] [--idle SECONDS]
"""

import argparse
import sqlite3
import sys
import time


def main():
    p = argparse.ArgumentParser()
    p.add_argument("db")
    p.add_argument("--mode", choices=["wal", "delete"], required=True)
    p.add_argument("--txs", type=int, default=20)
    p.add_argument("--rows", type=int, default=50)
    p.add_argument("--pause", type=float, default=0.25)
    p.add_argument("--idle", type=float, default=0.0)
    a = p.parse_args()

    con = sqlite3.connect(a.db, isolation_level=None)
    mode = con.execute(f"PRAGMA journal_mode={a.mode}").fetchone()[0]
    if mode.lower() != a.mode:
        sys.exit(f"journal_mode is {mode}, not {a.mode}")
    con.execute("PRAGMA synchronous=NORMAL")
    con.execute("CREATE TABLE IF NOT EXISTS t(id INTEGER PRIMARY KEY, tx INTEGER, v TEXT)")
    total = 0
    for tx in range(a.txs):
        con.execute("BEGIN")
        for i in range(a.rows):
            # Wide enough rows that a transaction spans several pages.
            con.execute("INSERT INTO t(tx, v) VALUES (?, ?)", (tx, f"{tx}-{i}-" + "x" * 200))
        con.execute("COMMIT")
        total = con.execute("SELECT count(*) FROM t").fetchone()[0]
        print(f"committed {total}", flush=True)
        time.sleep(a.pause)
    print(f"done {total}", flush=True)
    time.sleep(a.idle)
    con.close()


if __name__ == "__main__":
    main()
