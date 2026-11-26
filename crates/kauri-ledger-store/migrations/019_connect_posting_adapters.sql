
-- Connect all supported posting entry points to the shared engine.
-- Preserve existing function signatures, ownership and permissions.
-- Do not modify previously committed accounting records.


-- ============================================================
-- 1. Trusted posting adapter
-- ============================================================

CREATE OR REPLACE FUNCTION ledger.post_transaction(
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
LANGUAGE sql
VOLATILE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
    SELECT
        p.posted_transaction_id,
        p.replayed,
        p.stored_response_body
    FROM ledger.execute_posting(
        p_ledger_id,
        p_transaction_id,
        p_route_scope,
        p_idempotency_key,
        p_request_hash,
        p_response_body,
        p_effective_at,
        p_entries,
        NULL::uuid,
        FALSE
    ) AS p;
$$;

ALTER FUNCTION ledger.post_transaction(
    UUID, UUID, TEXT, TEXT,
    BYTEA, BYTEA, TIMESTAMPTZ, JSONB
)
OWNER TO ledger_migrator;

REVOKE ALL
ON FUNCTION ledger.post_transaction(
    UUID, UUID, TEXT, TEXT,
    BYTEA, BYTEA, TIMESTAMPTZ, JSONB
)
FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;


-- ============================================================
-- 2. Authorized HTTP posting adapter
-- ============================================================

CREATE OR REPLACE FUNCTION ledger.authorized_post_transaction(
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
VOLATILE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    -- Authorize the caller before accessing the posting engine.
    PERFORM ledger.require_api_access(
        p_prefix,
        p_key_hash,
        p_ledger_id,
        'write'
    );

    -- Prevent callers from supplying an unrelated route scope.
    IF p_route_scope IS DISTINCT FROM (
        'POST /v1/ledgers/'
        || p_ledger_id::text
        || '/transactions'
    ) THEN
        RAISE EXCEPTION 'INVALID_ROUTE_SCOPE'
            USING ERRCODE = '23514';
    END IF;

    -- Retain the response parameter for signature compatibility.
    -- The shared engine generates the authoritative HTTP response.
    RETURN QUERY
    SELECT
        p.posted_transaction_id,
        p.replayed,
        p.stored_response_body
    FROM ledger.execute_posting(
        p_ledger_id,
        p_transaction_id,
        p_route_scope,
        p_idempotency_key,
        p_request_hash,
        NULL::bytea,
        p_effective_at,
        p_entries,
        NULL::uuid,
        TRUE
    ) AS p;
END;
$$;

ALTER FUNCTION ledger.authorized_post_transaction(
    TEXT, BYTEA, UUID, UUID, TEXT,
    TEXT, BYTEA, BYTEA, TIMESTAMPTZ, JSONB
)
OWNER TO ledger_migrator;

REVOKE ALL
ON FUNCTION ledger.authorized_post_transaction(
    TEXT, BYTEA, UUID, UUID, TEXT,
    TEXT, BYTEA, BYTEA, TIMESTAMPTZ, JSONB
)
FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;

GRANT EXECUTE
ON FUNCTION ledger.authorized_post_transaction(
    TEXT, BYTEA, UUID, UUID, TEXT,
    TEXT, BYTEA, BYTEA, TIMESTAMPTZ, JSONB
)
TO ledger_app;


-- ============================================================
-- 3. Authorized reversal adapter
-- ============================================================

CREATE OR REPLACE FUNCTION ledger.authorized_reverse_transaction(
    p_prefix TEXT,
    p_key_hash BYTEA,
    p_ledger_id UUID,
    p_original_transaction_id UUID,
    p_reversal_transaction_id UUID,
    p_idempotency_key TEXT,
    p_request_hash BYTEA,
    p_effective_at TIMESTAMPTZ
)
RETURNS TABLE (
    posted_transaction_id UUID,
    replayed BOOLEAN,
    stored_response_body BYTEA
)
LANGUAGE plpgsql
VOLATILE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_route_scope TEXT;
BEGIN
    -- Authorization must also apply to idempotent replays.
    PERFORM ledger.require_api_access(
        p_prefix,
        p_key_hash,
        p_ledger_id,
        'write'
    );

    IF p_original_transaction_id IS NULL
       OR p_reversal_transaction_id IS NULL
       OR p_original_transaction_id = p_reversal_transaction_id
       OR p_effective_at IS NULL
    THEN
        RAISE EXCEPTION 'INVALID_REVERSAL_REQUEST'
            USING ERRCODE = '23514';
    END IF;

    -- Derive the canonical route from the original transaction.
    v_route_scope :=
        'POST /v1/ledgers/'
        || p_ledger_id::text
        || '/transactions/'
        || p_original_transaction_id::text
        || '/reverse';

    -- The shared engine derives the reversed journal entries.
    -- No caller-defined reversal entries are accepted.
    RETURN QUERY
    SELECT
        p.posted_transaction_id,
        p.replayed,
        p.stored_response_body
    FROM ledger.execute_posting(
        p_ledger_id,
        p_reversal_transaction_id,
        v_route_scope,
        p_idempotency_key,
        p_request_hash,
        NULL::bytea,
        p_effective_at,
        NULL::jsonb,
        p_original_transaction_id,
        TRUE
    ) AS p;
END;
$$;

ALTER FUNCTION ledger.authorized_reverse_transaction(
    TEXT, BYTEA, UUID, UUID, UUID,
    TEXT, BYTEA, TIMESTAMPTZ
)
OWNER TO ledger_migrator;

REVOKE ALL
ON FUNCTION ledger.authorized_reverse_transaction(
    TEXT, BYTEA, UUID, UUID, UUID,
    TEXT, BYTEA, TIMESTAMPTZ
)
FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;

GRANT EXECUTE
ON FUNCTION ledger.authorized_reverse_transaction(
    TEXT, BYTEA, UUID, UUID, UUID,
    TEXT, BYTEA, TIMESTAMPTZ
)
TO ledger_app;


-- ============================================================
-- 4. Preserve the private engine's security boundary
-- ============================================================

REVOKE ALL
ON FUNCTION ledger.execute_posting(
    UUID, UUID, TEXT, TEXT, BYTEA,
    BYTEA, TIMESTAMPTZ, JSONB, UUID, BOOLEAN
)
FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;


-- ============================================================
-- 5. Verify that all adapters delegate to the shared engine
-- ============================================================

DO $$
DECLARE
    v_signature REGPROCEDURE;
BEGIN
    FOR v_signature IN
        SELECT signature
        FROM (
            VALUES
            (
                'ledger.post_transaction(uuid,uuid,text,text,bytea,bytea,timestamptz,jsonb)'
                    ::regprocedure
            ),
            (
                'ledger.authorized_post_transaction(text,bytea,uuid,uuid,text,text,bytea,bytea,timestamptz,jsonb)'
                    ::regprocedure
            ),
            (
                'ledger.authorized_reverse_transaction(text,bytea,uuid,uuid,uuid,text,bytea,timestamptz)'
                    ::regprocedure
            )
        ) AS adapters(signature)
    LOOP
        IF strpos(
            pg_get_functiondef(v_signature::oid),
            'ledger.execute_posting('
        ) = 0 THEN
            RAISE EXCEPTION
                'POSTING_ADAPTER_NOT_CONNECTED: %',
                v_signature;
        END IF;

        IF NOT EXISTS (
            SELECT 1
            FROM pg_catalog.pg_proc AS p
            WHERE p.oid = v_signature::oid
              AND p.prosecdef
              AND pg_get_userbyid(p.proowner)
                  = 'ledger_migrator'
        ) THEN
            RAISE EXCEPTION
                'INVALID_POSTING_ADAPTER_SECURITY: %',
                v_signature;
        END IF;
    END LOOP;
END;
$$;
