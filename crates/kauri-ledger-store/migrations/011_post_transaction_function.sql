
-- Post a balanced accounting transaction and update account totals.
-- Execution is restricted to trusted roles until tenant authorization
-- is enforced at the runtime boundary.

CREATE FUNCTION ledger.post_transaction(
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
    v_existing_id UUID;
    v_existing_hash BYTEA;
    v_existing_body BYTEA;
    v_inserted_id UUID;
    v_entry_count INTEGER;
    v_debit_sum NUMERIC;
    v_credit_sum NUMERIC;
    v_account_status TEXT;
    v_delta RECORD;
BEGIN
    -- Fast path: replay a previously committed request.
    -- This remains available even if the ledger was frozen later.
    SELECT
        t.id,
        t.request_hash,
        t.response_body
    INTO
        v_existing_id,
        v_existing_hash,
        v_existing_body
    FROM ledger.transactions AS t
    WHERE t.ledger_id = p_ledger_id
      AND t.route_scope = p_route_scope
      AND t.idempotency_key = p_idempotency_key;

    IF FOUND THEN
        IF v_existing_hash IS DISTINCT FROM p_request_hash THEN
            RAISE EXCEPTION
                'IDEMPOTENCY_KEY_REUSED'
                USING ERRCODE = '23514';
        END IF;

        RETURN QUERY
        SELECT v_existing_id, TRUE, v_existing_body;
        RETURN;
    END IF;

    -- Validate the new request before acquiring account locks.
    IF p_entries IS NULL
       OR jsonb_typeof(p_entries) IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION
            'INVALID_ENTRIES: expected a JSON array'
            USING ERRCODE = '23514';
    END IF;

    v_entry_count := jsonb_array_length(p_entries);

    IF v_entry_count < 2 OR v_entry_count > 64 THEN
        RAISE EXCEPTION
            'INVALID_ENTRY_COUNT: %',
            v_entry_count
            USING ERRCODE = '23514';
    END IF;

    IF p_request_hash IS NULL
       OR octet_length(p_request_hash) <> 32 THEN
        RAISE EXCEPTION
            'INVALID_REQUEST_HASH'
            USING ERRCODE = '23514';
    END IF;

    IF EXISTS (
        SELECT 1
        FROM jsonb_to_recordset(p_entries) AS e(
            account_id UUID,
            side TEXT,
            amount BIGINT
        )
        WHERE e.account_id IS NULL
           OR e.side IS NULL
           OR e.side NOT IN ('D', 'C')
           OR e.amount IS NULL
           OR e.amount < 1
           OR e.amount > 1000000000000
    ) THEN
        RAISE EXCEPTION
            'INVALID_ENTRY_DATA'
            USING ERRCODE = '23514';
    END IF;

    SELECT
        COALESCE(
            SUM(
                CASE WHEN e.side = 'D'
                    THEN e.amount::NUMERIC ELSE 0 END
            ),
            0
        ),
        COALESCE(
            SUM(
                CASE WHEN e.side = 'C'
                    THEN e.amount::NUMERIC ELSE 0 END
            ),
            0
        )
    INTO v_debit_sum, v_credit_sum
    FROM jsonb_to_recordset(p_entries) AS e(
        account_id UUID,
        side TEXT,
        amount BIGINT
    );

    IF v_debit_sum <> v_credit_sum THEN
        RAISE EXCEPTION
            'UNBALANCED_TRANSACTION'
            USING ERRCODE = '23514';
    END IF;

    -- The unique index serializes concurrent requests with
    -- the same ledger, route and idempotency key.
    INSERT INTO ledger.transactions (
        ledger_id,
        id,
        route_scope,
        idempotency_key,
        request_hash,
        response_snapshot,
        response_body,
        effective_at
    )
    VALUES (
        p_ledger_id,
        p_transaction_id,
        p_route_scope,
        p_idempotency_key,
        p_request_hash,
        convert_from(p_response_body, 'UTF8')::jsonb,
        p_response_body,
        p_effective_at
    )
    ON CONFLICT ON CONSTRAINT transactions_idempotency_unique
    DO NOTHING
    RETURNING id INTO v_inserted_id;

    -- A competing request committed first.
    -- Read its committed response in a fresh statement.
    IF v_inserted_id IS NULL THEN
        SELECT
            t.id,
            t.request_hash,
            t.response_body
        INTO
            v_existing_id,
            v_existing_hash,
            v_existing_body
        FROM ledger.transactions AS t
        WHERE t.ledger_id = p_ledger_id
          AND t.route_scope = p_route_scope
          AND t.idempotency_key = p_idempotency_key;

        IF NOT FOUND THEN
            RAISE EXCEPTION
                'IDEMPOTENCY_RETRY_REQUIRED'
                USING ERRCODE = '40001';
        END IF;

        IF v_existing_hash IS DISTINCT FROM p_request_hash THEN
            RAISE EXCEPTION
                'IDEMPOTENCY_KEY_REUSED'
                USING ERRCODE = '23514';
        END IF;

        RETURN QUERY
        SELECT v_existing_id, TRUE, v_existing_body;
        RETURN;
    END IF;

    -- Aggregate repeated entries for the same account.
    -- Acquire every balance lock in ascending account UUID order.
    FOR v_delta IN
        SELECT
            e.account_id,
            SUM(
                CASE WHEN e.side = 'D'
                    THEN e.amount ELSE 0 END
            )::BIGINT AS debit_change,
            SUM(
                CASE WHEN e.side = 'C'
                    THEN e.amount ELSE 0 END
            )::BIGINT AS credit_change
        FROM jsonb_to_recordset(p_entries) AS e(
            account_id UUID,
            side TEXT,
            amount BIGINT
        )
        GROUP BY e.account_id
        ORDER BY e.account_id
    LOOP
        SELECT a.status
        INTO v_account_status
        FROM ledger.account_balances AS b
        JOIN ledger.accounts AS a
          ON a.ledger_id = b.ledger_id
         AND a.id = b.account_id
        WHERE b.ledger_id = p_ledger_id
          AND b.account_id = v_delta.account_id
        FOR UPDATE OF b;

        IF NOT FOUND THEN
            RAISE EXCEPTION
                'ACCOUNT_BALANCE_NOT_FOUND: %',
                v_delta.account_id
                USING ERRCODE = '23503';
        END IF;

        IF v_account_status <> 'active' THEN
            RAISE EXCEPTION
                'ACCOUNT_CLOSED: %',
                v_delta.account_id
                USING ERRCODE = '23514';
        END IF;

        -- Table CHECK constraints enforce total limits and
        -- the account's available-balance policy.
        UPDATE ledger.account_balances AS b
        SET
            debit_total = b.debit_total + v_delta.debit_change,
            credit_total = b.credit_total + v_delta.credit_change,
            version = b.version + 1,
            updated_at = NOW()
        WHERE b.ledger_id = p_ledger_id
          AND b.account_id = v_delta.account_id;
    END LOOP;

    -- Preserve input entry order in the append-only journal.
    INSERT INTO ledger.entries (
        ledger_id,
        transaction_id,
        account_id,
        side,
        amount,
        effective_at
    )
    SELECT
        p_ledger_id,
        v_inserted_id,
        (e.value->>'account_id')::UUID,
        (e.value->>'side')::CHAR(1),
        (e.value->>'amount')::BIGINT,
        p_effective_at
    FROM jsonb_array_elements(p_entries)
        WITH ORDINALITY AS e(value, position)
    ORDER BY e.position;

    -- The deferred trigger validates and seals this transaction
    -- when the surrounding SQL transaction commits.
    RETURN QUERY
    SELECT v_inserted_id, FALSE, p_response_body;
END;
$$;

ALTER FUNCTION ledger.post_transaction(
    UUID, UUID, TEXT, TEXT, BYTEA, BYTEA, TIMESTAMPTZ, JSONB
)
OWNER TO ledger_migrator;

REVOKE ALL
ON FUNCTION ledger.post_transaction(
    UUID, UUID, TEXT, TEXT, BYTEA, BYTEA, TIMESTAMPTZ, JSONB
)
FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;
