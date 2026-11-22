
-- Store immutable, tenant-scoped accounting transactions.
CREATE TABLE ledger.transactions (
    ledger_id UUID NOT NULL,
    id UUID NOT NULL,

    reference TEXT,

    -- The scope identifies the HTTP method and concrete request path.
    route_scope TEXT NOT NULL,

    idempotency_key TEXT NOT NULL,
    request_hash BYTEA NOT NULL,

    -- JSONB supports inspection; bytes preserve exact replay.
    response_snapshot JSONB NOT NULL,
    response_body BYTEA NOT NULL,

    reverses_transaction UUID,

    effective_at TIMESTAMPTZ NOT NULL,
    posted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,

    CONSTRAINT transactions_pk
        PRIMARY KEY (ledger_id, id),

    CONSTRAINT transactions_ledger_fk
        FOREIGN KEY (ledger_id)
        REFERENCES ledger.ledgers (id)
        ON DELETE RESTRICT,

    CONSTRAINT transactions_effective_at_unique
        UNIQUE (ledger_id, id, effective_at),

    CONSTRAINT transactions_idempotency_unique
        UNIQUE (ledger_id, route_scope, idempotency_key),

    CONSTRAINT transactions_route_scope_valid
        CHECK (char_length(route_scope) BETWEEN 1 AND 512),

    CONSTRAINT transactions_idempotency_key_valid
        CHECK (char_length(idempotency_key) BETWEEN 8 AND 200),

    CONSTRAINT transactions_request_hash_valid
        CHECK (octet_length(request_hash) = 32),

    CONSTRAINT transactions_reference_valid
        CHECK (reference IS NULL OR char_length(reference) <= 200),

    CONSTRAINT transactions_metadata_valid
        CHECK (
            jsonb_typeof(metadata) = 'object'
            AND octet_length(metadata::text) <= 4096
        ),

    CONSTRAINT transactions_response_consistent
        CHECK (
            response_snapshot =
                convert_from(response_body, 'UTF8')::jsonb
        ),

    CONSTRAINT transactions_reversal_not_self
        CHECK (
            reverses_transaction IS NULL
            OR reverses_transaction <> id
        ),

    CONSTRAINT transactions_reversal_fk
        FOREIGN KEY (ledger_id, reverses_transaction)
        REFERENCES ledger.transactions (ledger_id, id)
        ON DELETE RESTRICT
);

-- A transaction may reverse another transaction only once.
CREATE UNIQUE INDEX transactions_reverses_once
    ON ledger.transactions (ledger_id, reverses_transaction)
    WHERE reverses_transaction IS NOT NULL;

CREATE INDEX transactions_ref
    ON ledger.transactions (ledger_id, reference)
    WHERE reference IS NOT NULL;

CREATE INDEX transactions_cursor
    ON ledger.transactions (ledger_id, effective_at, id);

-- Entries reference transactions and accounts within the same ledger.
CREATE TABLE ledger.entries (
    id BIGINT GENERATED ALWAYS AS IDENTITY,

    ledger_id UUID NOT NULL,
    transaction_id UUID NOT NULL,
    account_id UUID NOT NULL,

    side CHAR(1) NOT NULL,
    amount BIGINT NOT NULL,

    currency CHAR(3) NOT NULL DEFAULT 'XOF',
    effective_at TIMESTAMPTZ NOT NULL,

    CONSTRAINT entries_pk
        PRIMARY KEY (ledger_id, id),

    CONSTRAINT entries_transaction_fk
        FOREIGN KEY (
            ledger_id,
            transaction_id,
            effective_at
        )
        REFERENCES ledger.transactions (
            ledger_id,
            id,
            effective_at
        )
        ON DELETE RESTRICT,

    CONSTRAINT entries_account_fk
        FOREIGN KEY (ledger_id, account_id)
        REFERENCES ledger.accounts (ledger_id, id)
        ON DELETE RESTRICT,

    CONSTRAINT entries_side_valid
        CHECK (side IN ('D', 'C')),

    CONSTRAINT entries_amount_valid
        CHECK (amount BETWEEN 1 AND 1000000000000),

    CONSTRAINT entries_currency_xof
        CHECK (currency = 'XOF')
);

CREATE INDEX entries_account_time
    ON ledger.entries (
        ledger_id,
        account_id,
        effective_at,
        id
    );

CREATE INDEX entries_tx
    ON ledger.entries (
        ledger_id,
        transaction_id
    );
