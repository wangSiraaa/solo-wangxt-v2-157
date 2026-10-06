#!/usr/bin/env bash
# Start the user-space PostgreSQL bundled under .pgsql/ (no root required).
# The application creates its own database on first connect.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$ROOT/.pgsql/bin:$PATH"
DATA="$ROOT/.pgdata/data"
PORT="${PGPORT:-54329}"

if [[ ! -d "$DATA" ]]; then
  mkdir -p "$ROOT/.pgdata"
  initdb -D "$DATA" -U postgres --locale=C --encoding=UTF8 >/dev/null
fi

pg_ctl -D "$DATA" -l "$ROOT/.pgdata/server.log" \
  -o "-p $PORT -k /tmp -c listen_addresses=127.0.0.1" \
  -w start

echo "postgres ready on 127.0.0.1:$PORT (data: $DATA)"
