
-- API key storage and database-enforced tenant authorization.
-- Provision ledger_migrator and ledger_app before this migration.

CREATE TABLE ledger.api_keys (
    id UUID PRIMARY KEY,
    key_hash BYTEA NOT NULL UNIQUE,
    prefix TEXT NOT NULL,
    last4 TEXT NOT NULL,
    scopes TEXT[] NOT NULL,
    label TEXT NOT NULL,
    created_by TEXT NOT NULL DEFAULT 'cli',
    expires_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ,
    ip_allowlist CIDR[],
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT api_keys_hash_valid
        CHECK (octet_length(key_hash) = 32),

    CONSTRAINT api_keys_prefix_valid
        CHECK (
            prefix ~ '^lk_(live|test)_[A-Za-z0-9]{8}$'
        ),

    CONSTRAINT api_keys_last4_valid
        CHECK (last4 ~ '^[A-Za-z0-9]{4}$'),

    CONSTRAINT api_keys_scopes_valid
        CHECK (
            cardinality(scopes) BETWEEN 1 AND 4
            AND scopes <@ ARRAY[
                'read', 'write', 'recon', 'admin'
            ]::TEXT[]
        ),

    CONSTRAINT api_keys_label_valid
        CHECK (
            char_length(label) BETWEEN 1 AND 200
            AND char_length(btrim(label)) > 0
        )
);

CREATE INDEX api_keys_prefix_lookup
    ON ledger.api_keys (prefix);

CREATE TABLE ledger.api_key_ledgers (
    api_key_id UUID NOT NULL
        REFERENCES ledger.api_keys (id)
        ON DELETE RESTRICT,

    ledger_id UUID NOT NULL
        REFERENCES ledger.ledgers (id)
        ON DELETE RESTRICT,

    CONSTRAINT api_key_ledgers_pk
        PRIMARY KEY (api_key_id, ledger_id)
);

-- Enforce the v1 one-ledger-per-key rule in PostgreSQL.
CREATE UNIQUE INDEX api_key_single_ledger_v1
    ON ledger.api_key_ledgers (api_key_id);

CREATE INDEX api_key_ledgers_ledger_lookup
    ON ledger.api_key_ledgers (ledger_id);

ALTER TABLE ledger.api_keys OWNER TO ledger_migrator;
ALTER TABLE ledger.api_key_ledgers OWNER TO ledger_migrator;

REVOKE ALL ON ledger.api_keys, ledger.api_key_ledgers
FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;


-- Compare 32-byte hashes without short-circuiting byte comparisons.
-- A matching prefix is only a lookup hint, never authentication.
CREATE FUNCTION ledger.api_key_access(
    p_prefix TEXT,
    p_key_hash BYTEA,
    p_ledger_id UUID,
    p_required_scope TEXT
)
RETURNS TEXT
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_key RECORD;
    v_diff INTEGER;
    v_position INTEGER;
BEGIN
    IF p_prefix IS NULL
       OR p_key_hash IS NULL
       OR octet_length(p_key_hash) <> 32
       OR p_required_scope IS NULL
       OR p_required_scope NOT IN ('read', 'write', 'recon')
    THEN
        RETURN 'unauthenticated';
    END IF;

    -- Hold a shared lock so revocation cannot overtake a
    -- posting already authorized in the same transaction.
    FOR v_key IN
        SELECT
            k.id,
            k.key_hash,
            k.scopes,
            k.expires_at,
            k.revoked_at
        FROM ledger.api_keys AS k
        WHERE k.prefix = p_prefix
        FOR SHARE
    LOOP
        v_diff := 0;

        FOR v_position IN 0..31 LOOP
            v_diff := v_diff | (
                get_byte(v_key.key_hash, v_position)
                # get_byte(p_key_hash, v_position)
            );
        END LOOP;

        IF v_diff = 0 THEN
            IF v_key.revoked_at IS NOT NULL
               OR (
                   v_key.expires_at IS NOT NULL
                   AND v_key.expires_at <= NOW()
               )
            THEN
                RETURN 'unauthenticated';
            END IF;

            -- Never reveal the existence of another tenant.
            IF NOT EXISTS (
                SELECT 1
                FROM ledger.api_key_ledgers AS kl
                WHERE kl.api_key_id = v_key.id
                  AND kl.ledger_id = p_ledger_id
            ) THEN
                RETURN 'not_found';
            END IF;

            IF NOT (p_required_scope = ANY(v_key.scopes)) THEN
                RETURN 'forbidden';
            END IF;

            RETURN 'allowed';
        END IF;
    END LOOP;

    RETURN 'unauthenticated';
END;
$$;

ALTER FUNCTION ledger.api_key_access(
    TEXT, BYTEA, UUID, TEXT
)
OWNER TO ledger_migrator;

REVOKE ALL ON FUNCTION ledger.api_key_access(
    TEXT, BYTEA, UUID, TEXT
)
FROM PUBLIC, ledger_admin, ledger_readonly;

GRANT EXECUTE ON FUNCTION ledger.api_key_access(
    TEXT, BYTEA, UUID, TEXT
)
TO ledger_app;


-- Internal authorization guard for privileged wrappers.
CREATE FUNCTION ledger.require_api_access(
    p_prefix TEXT,
    p_key_hash BYTEA,
    p_ledger_id UUID,
    p_required_scope TEXT
)
RETURNS VOID
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_access TEXT;
BEGIN
    v_access := ledger.api_key_access(
        p_prefix,
        p_key_hash,
        p_ledger_id,
        p_required_scope
    );

    CASE v_access
        WHEN 'allowed' THEN
            RETURN;

        WHEN 'not_found' THEN
            RAISE EXCEPTION 'LEDGER_NOT_FOUND'
                USING ERRCODE = 'P0002';

        WHEN 'forbidden' THEN
            RAISE EXCEPTION 'INSUFFICIENT_SCOPE'
                USING ERRCODE = '42501';

        ELSE
            RAISE EXCEPTION 'INVALID_API_KEY'
                USING ERRCODE = '28000';
    END CASE;
END;
$$;

ALTER FUNCTION ledger.require_api_access(
    TEXT, BYTEA, UUID, TEXT
)
OWNER TO ledger_migrator;

REVOKE ALL ON FUNCTION ledger.require_api_access(
    TEXT, BYTEA, UUID, TEXT
)
FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;


-- Authorized wrapper around the existing posting function.
-- The underlying privileged function stays inaccessible to ledger_app.
CREATE FUNCTION ledger.authorized_post_transaction(
    p_prefix TEXT,
    p_key_hash BYTEA,
    p_ledger_id UUID,
    p_transaction_id UUID,
    p_route_scope TEXT,
    p_idempotency_key TEXT,
    p_request_hash BYTEA,
    p_response_body BYTEA,
    p_effective_at TIMESTAMPTZ,
    p_entries JSONB
)
RETURNS TABLE (
    posted_transaction_id UUID,
    replayed BOOLEAN,
    stored_response_body BYTEA
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    PERFORM ledger.require_api_access(
        p_prefix,
        p_key_hash,
        p_ledger_id,
        'write'
    );

    RETURN QUERY
    SELECT
        p.posted_transaction_id,
        p.replayed,
        p.stored_response_body
    FROM ledger.post_transaction(
        p_ledger_id,
        p_transaction_id,
        p_route_scope,
        p_idempotency_key,
        p_request_hash,
        p_response_body,
        p_effective_at,
        p_entries
    ) AS p;
END;
$$;

ALTER FUNCTION ledger.authorized_post_transaction(
    TEXT, BYTEA, UUID, UUID, TEXT,
    TEXT, BYTEA, BYTEA, TIMESTAMPTZ, JSONB
)
OWNER TO ledger_migrator;

REVOKE ALL ON FUNCTION ledger.authorized_post_transaction(
    TEXT, BYTEA, UUID, UUID, TEXT,
    TEXT, BYTEA, BYTEA, TIMESTAMPTZ, JSONB
)
FROM PUBLIC, ledger_admin, ledger_readonly;

GRANT EXECUTE ON FUNCTION ledger.authorized_post_transaction(
    TEXT, BYTEA, UUID, UUID, TEXT,
    TEXT, BYTEA, BYTEA, TIMESTAMPTZ, JSONB
)
TO ledger_app;


-- Read balances only through a scoped, authorized function.
CREATE FUNCTION ledger.authorized_get_balance(
    p_prefix TEXT,
    p_key_hash BYTEA,
    p_ledger_id UUID,
    p_account_code TEXT
)
RETURNS TABLE (
    account_id UUID,
    account_code TEXT,
    account_status TEXT,
    posted_total BIGINT,
    reserved_total BIGINT,
    available_total BIGINT,
    balance_version BIGINT
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    PERFORM ledger.require_api_access(
        p_prefix,
        p_key_hash,
        p_ledger_id,
        'read'
    );

    RETURN QUERY
    SELECT
        a.id,
        a.code,
        a.status,
        CASE
            WHEN b.normal_side = 'D'
                THEN b.debit_total - b.credit_total
            ELSE b.credit_total - b.debit_total
        END,
        b.reserved_total,
        CASE
            WHEN b.normal_side = 'D'
                THEN b.debit_total - b.credit_total
            ELSE b.credit_total - b.debit_total
        END - b.reserved_total,
        b.version
    FROM ledger.accounts AS a
    JOIN ledger.account_balances AS b
      ON b.ledger_id = a.ledger_id
     AND b.account_id = a.id
    WHERE a.ledger_id = p_ledger_id
      AND a.code = p_account_code;
END;
$$;

ALTER FUNCTION ledger.authorized_get_balance(
    TEXT, BYTEA, UUID, TEXT
)
OWNER TO ledger_migrator;

REVOKE ALL ON FUNCTION ledger.authorized_get_balance(
    TEXT, BYTEA, UUID, TEXT
)
FROM PUBLIC, ledger_admin, ledger_readonly;

GRANT EXECUTE ON FUNCTION ledger.authorized_get_balance(
    TEXT, BYTEA, UUID, TEXT
)
TO ledger_app;
