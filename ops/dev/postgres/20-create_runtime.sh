#!/usr/bin/env bash
set -euo pipefail

: "${POSTGRES_USER:?POSTGRES_USER is required}"
: "${POSTGRES_DB:?POSTGRES_DB is required}"
: "${LEDGER_HTTP_PASSWORD:?LEDGER_HTTP_PASSWORD is required}"

psql \
    --username "$POSTGRES_USER" \
    --dbname "$POSTGRES_DB" \
    --set ON_ERROR_STOP=1 \
    --set runtime_password="$LEDGER_HTTP_PASSWORD" <<'SQL'

CREATE ROLE ledger_http
    LOGIN
    INHERIT
    NOSUPERUSER
    NOCREATEDB
    NOCREATEROLE
    NOREPLICATION
    NOBYPASSRLS
    PASSWORD :'runtime_password';

GRANT ledger_app TO ledger_http;

SQL