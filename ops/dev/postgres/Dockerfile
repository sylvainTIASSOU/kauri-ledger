FROM postgres:17.12-bookworm

LABEL org.opencontainers.image.title="Kauri Ledger PostgreSQL Dev"
LABEL org.opencontainers.image.description="Local PostgreSQL development database for Kauri Ledger"

COPY ops/postgres/bootstrap_roles.sql \
    /docker-entrypoint-initdb.d/10-bootstrap_roles.sql

COPY ops/dev/postgres/20-create_runtime.sh \
    /docker-entrypoint-initdb.d/20-create_runtime.sh

RUN chmod 0755 \
    /docker-entrypoint-initdb.d/20-create_runtime.sh