-- Produce the immutable HTTP response before the accounting transaction commits.
-- Existing trusted callers of ledger.post_transaction remain unchanged.

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
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_existing_hash BYTEA;
    v_delta RECORD;
    v_balance RECORD;
    v_debit BIGINT;
    v_credit BIGINT;
    v_posted BIGINT;
    v_available BIGINT;
    v_balances JSONB := '[]'::jsonb;
    v_response BYTEA;
BEGIN
    PERFORM ledger.require_api_access(
        p_prefix, p_key_hash, p_ledger_id, 'write'
    );

    IF p_route_scope IS DISTINCT FROM
       ('POST /v1/ledgers/' || p_ledger_id::text || '/transactions')
    THEN
        RAISE EXCEPTION 'INVALID_ROUTE_SCOPE'
            USING ERRCODE = '23514';
    END IF;

    -- An idempotent replay must not depend on current balances or ledger status.
    SELECT t.request_hash INTO v_existing_hash
    FROM ledger.transactions AS t
    WHERE t.ledger_id = p_ledger_id
      AND t.route_scope = p_route_scope
      AND t.idempotency_key = p_idempotency_key;

    IF FOUND THEN
        IF v_existing_hash IS DISTINCT FROM p_request_hash THEN
            RAISE EXCEPTION 'IDEMPOTENCY_KEY_REUSED'
                USING ERRCODE = '23514';
        END IF;

        RETURN QUERY
        SELECT p.posted_transaction_id, p.replayed, p.stored_response_body
        FROM ledger.post_transaction(
            p_ledger_id, p_transaction_id, p_route_scope,
            p_idempotency_key, p_request_hash, '{}'::bytea,
            p_effective_at, p_entries
        ) AS p;
        RETURN;
    END IF;

    -- Follow the established lock order: parent ledger before account balances.
    PERFORM 1 FROM ledger.ledgers AS l
    WHERE l.id = p_ledger_id FOR SHARE;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'LEDGER_NOT_FOUND' USING ERRCODE = 'P0002';
    END IF;

    -- Lock every affected balance in ascending UUID order, just like the
    -- underlying posting function. All calculated balances are provisional
    -- until the same SQL transaction successfully commits.
    FOR v_delta IN
        SELECT e.account_id,
            SUM(CASE WHEN e.side = 'D' THEN e.amount ELSE 0 END)::bigint
                AS debit_change,
            SUM(CASE WHEN e.side = 'C' THEN e.amount ELSE 0 END)::bigint
                AS credit_change
        FROM jsonb_to_recordset(p_entries) AS e(
            account_id UUID, side TEXT, amount BIGINT
        )
        GROUP BY e.account_id
        ORDER BY e.account_id
    LOOP
        SELECT b.debit_total, b.credit_total, b.reserved_total,
               b.normal_side, b.version, a.code
        INTO v_balance
        FROM ledger.account_balances AS b
        JOIN ledger.accounts AS a
          ON a.ledger_id = b.ledger_id AND a.id = b.account_id
        WHERE b.ledger_id = p_ledger_id
          AND b.account_id = v_delta.account_id
        FOR UPDATE OF b;

        IF NOT FOUND THEN
            RAISE EXCEPTION 'ACCOUNT_BALANCE_NOT_FOUND'
                USING ERRCODE = '23503';
        END IF;

        v_debit := v_balance.debit_total + v_delta.debit_change;
        v_credit := v_balance.credit_total + v_delta.credit_change;
        v_posted := CASE WHEN v_balance.normal_side = 'D'
            THEN v_debit - v_credit ELSE v_credit - v_debit END;
        v_available := v_posted - v_balance.reserved_total;

        -- Amounts use JSON strings to avoid loss of 64-bit precision in JS.
        v_balances := v_balances || jsonb_build_array(jsonb_build_object(
            'account_id', v_delta.account_id,
            'account_code', v_balance.code,
            'posted_total', v_posted::text,
            'reserved_total', v_balance.reserved_total::text,
            'available_total', v_available::text,
            'version', v_balance.version + 1
        ));
    END LOOP;

    v_response := convert_to(jsonb_build_object(
        'status', 'posted',
        'transaction_id', p_transaction_id,
        'balances_after', v_balances
    )::text, 'UTF8');

    -- The original posting function enforces all business invariants and
    -- inserts v_response into the idempotency row in the same transaction.
    -- For a concurrent duplicate, it returns the previously stored bytes.
    RETURN QUERY
    SELECT p.posted_transaction_id, p.replayed, p.stored_response_body
    FROM ledger.post_transaction(
        p_ledger_id, p_transaction_id, p_route_scope,
        p_idempotency_key, p_request_hash, v_response,
        p_effective_at, p_entries
    ) AS p;
END;
$$;

ALTER FUNCTION ledger.authorized_post_transaction(
    TEXT, BYTEA, UUID, UUID, TEXT, TEXT, BYTEA, BYTEA, TIMESTAMPTZ, JSONB
) OWNER TO ledger_migrator;

REVOKE ALL ON FUNCTION ledger.authorized_post_transaction(
    TEXT, BYTEA, UUID, UUID, TEXT, TEXT, BYTEA, BYTEA, TIMESTAMPTZ, JSONB
) FROM PUBLIC, ledger_admin, ledger_readonly;

GRANT EXECUTE ON FUNCTION ledger.authorized_post_transaction(
    TEXT, BYTEA, UUID, UUID, TEXT, TEXT, BYTEA, BYTEA, TIMESTAMPTZ, JSONB
) TO ledger_app;

-- Scoped transaction lookup for the read-only API.
CREATE FUNCTION ledger.authorized_get_transaction(
    p_prefix TEXT, p_key_hash BYTEA,
    p_ledger_id UUID, p_transaction_id UUID
)
RETURNS TABLE (
    transaction_id UUID,
    posted_at TIMESTAMPTZ,
    effective_at TIMESTAMPTZ,
    response_body BYTEA
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    PERFORM ledger.require_api_access(
        p_prefix, p_key_hash, p_ledger_id, 'read'
    );

    RETURN QUERY
    SELECT t.id, t.posted_at, t.effective_at, t.response_body
    FROM ledger.transactions AS t
    WHERE t.ledger_id = p_ledger_id
      AND t.id = p_transaction_id;
END;
$$;

ALTER FUNCTION ledger.authorized_get_transaction(
    TEXT, BYTEA, UUID, UUID
) OWNER TO ledger_migrator;

REVOKE ALL ON FUNCTION ledger.authorized_get_transaction(
    TEXT, BYTEA, UUID, UUID
) FROM PUBLIC, ledger_admin, ledger_readonly;

GRANT EXECUTE ON FUNCTION ledger.authorized_get_transaction(
    TEXT, BYTEA, UUID, UUID
) TO ledger_app;
