
-- Centralize accounting writes behind one private posting engine.
-- All supported posting paths acquire locks in the same order.
-- Existing committed transactions and response bytes remain unchanged.

CREATE FUNCTION ledger.execute_posting(
    p_ledger_id UUID,
    p_transaction_id UUID,
    p_route_scope TEXT,
    p_idempotency_key TEXT,
    p_request_hash BYTEA,
    p_response_body BYTEA,
    p_effective_at TIMESTAMPTZ,
    p_entries JSONB,
    p_original_id UUID,
    p_server_response BOOLEAN
)
RETURNS TABLE (
    posted_transaction_id UUID,
    replayed BOOLEAN,
    stored_response_body BYTEA
)
LANGUAGE plpgsql
VOLATILE
SECURITY INVOKER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_lock_key BIGINT;

    v_existing RECORD;
    v_ledger_status TEXT;
    v_period_cutoff TIMESTAMPTZ;

    v_entries JSONB;
    v_entry_count INTEGER;
    v_debit_sum NUMERIC;
    v_credit_sum NUMERIC;

    v_delta RECORD;
    v_balance RECORD;
    v_account_code TEXT;
    v_account_status TEXT;

    v_posted BIGINT;
    v_available BIGINT;

    v_balances JSONB := '[]'::jsonb;
    v_snapshot JSONB;
    v_response BYTEA;
BEGIN
    -- Validate the logical operation identity.
    IF p_ledger_id IS NULL
       OR p_route_scope IS NULL
       OR char_length(p_route_scope) NOT BETWEEN 1 AND 512
       OR p_idempotency_key IS NULL
       OR char_length(p_idempotency_key) NOT BETWEEN 8 AND 200
       OR p_request_hash IS NULL
       OR octet_length(p_request_hash) <> 32
    THEN
        RAISE EXCEPTION 'INVALID_POSTING_REQUEST'
            USING ERRCODE = '23514';
    END IF;

    -- Regular postings cannot occupy the reversal route namespace.
    IF p_original_id IS NULL
       AND p_route_scope LIKE
           'POST /v1/ledgers/%/transactions/%/reverse'
    THEN
        RAISE EXCEPTION 'INVALID_ROUTE_SCOPE'
            USING ERRCODE = '23514';
    END IF;

    -- Acquire an operation-specific transaction-level advisory lock.
    -- JSON encoding avoids ambiguous concatenation of key components.
    v_lock_key := pg_catalog.hashtextextended(
        jsonb_build_array(
            'kauri-ledger:posting:v1',
            p_ledger_id,
            p_route_scope,
            p_idempotency_key
        )::text,
        0::bigint
    );

    PERFORM pg_catalog.pg_advisory_xact_lock(v_lock_key);

    -- Resolve idempotency before inspecting mutable accounting state.
    SELECT
        t.id,
        t.request_hash,
        t.response_body,
        t.reverses_transaction
    INTO v_existing
    FROM ledger.transactions AS t
    WHERE t.ledger_id = p_ledger_id
      AND t.route_scope = p_route_scope
      AND t.idempotency_key = p_idempotency_key;

    IF FOUND THEN
        IF v_existing.request_hash IS DISTINCT FROM p_request_hash THEN
            RAISE EXCEPTION 'IDEMPOTENCY_KEY_REUSED'
                USING ERRCODE = '23514';
        END IF;

        IF v_existing.reverses_transaction
           IS DISTINCT FROM p_original_id
        THEN
            RAISE EXCEPTION 'INVALID_ROUTE_SCOPE'
                USING ERRCODE = '23514';
        END IF;

        RETURN QUERY
        SELECT
            v_existing.id,
            TRUE,
            v_existing.response_body;

        RETURN;
    END IF;

    -- Validate fields needed only for a new transaction.
    IF p_transaction_id IS NULL
       OR p_effective_at IS NULL
       OR p_server_response IS NULL
       OR p_transaction_id = p_original_id
       OR (
           NOT p_server_response
           AND p_response_body IS NULL
       )
    THEN
        RAISE EXCEPTION 'INVALID_POSTING_REQUEST'
            USING ERRCODE = '23514';
    END IF;

    -- Lock the parent ledger before source transactions or balances.
    SELECT l.status
    INTO v_ledger_status
    FROM ledger.ledgers AS l
    WHERE l.id = p_ledger_id
    FOR SHARE;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'LEDGER_NOT_FOUND'
            USING ERRCODE = 'P0002';
    END IF;

    -- Regular postings use their validated input entries.
    v_entries := p_entries;

    IF p_original_id IS NOT NULL THEN
        -- Serialize distinct reversal requests for the same source.
        PERFORM 1
        FROM ledger.transactions AS t
        WHERE t.ledger_id = p_ledger_id
          AND t.id = p_original_id
        FOR UPDATE;

        IF NOT FOUND THEN
            RAISE EXCEPTION 'TRANSACTION_NOT_FOUND'
                USING ERRCODE = 'P0002';
        END IF;

        IF EXISTS (
            SELECT 1
            FROM ledger.transactions AS t
            WHERE t.ledger_id = p_ledger_id
              AND t.reverses_transaction = p_original_id
        ) THEN
            RAISE EXCEPTION 'ALREADY_REVERSED'
                USING ERRCODE = '23514';
        END IF;

        IF NOT EXISTS (
            SELECT 1
            FROM ledger.transaction_seals AS s
            WHERE s.ledger_id = p_ledger_id
              AND s.transaction_id = p_original_id
        ) THEN
            RAISE EXCEPTION 'TRANSACTION_NOT_SEALED'
                USING ERRCODE = '23514';
        END IF;

        -- Derive reversal entries from the immutable source journal.
        -- Never accept client-defined reversal entries.
        SELECT COALESCE(
            jsonb_agg(
                jsonb_build_object(
                    'account_id', e.account_id,
                    'side', CASE
                        WHEN e.side = 'D' THEN 'C'
                        WHEN e.side = 'C' THEN 'D'
                        ELSE NULL
                    END,
                    'amount', e.amount
                )
                ORDER BY e.id
            ),
            '[]'::jsonb
        )
        INTO v_entries
        FROM ledger.entries AS e
        WHERE e.ledger_id = p_ledger_id
          AND e.transaction_id = p_original_id;
    END IF;

    -- Validate ledger state before changing account balances.
    -- The existing INSERT trigger independently enforces these rules.
    IF v_ledger_status = 'frozen' THEN
        RAISE EXCEPTION 'LEDGER_FROZEN'
            USING ERRCODE = '23514';
    END IF;

    SELECT l.locked_until
    INTO v_period_cutoff
    FROM ledger.period_locks AS l
    WHERE l.ledger_id = p_ledger_id;

    IF v_period_cutoff IS NOT NULL
       AND p_effective_at <= v_period_cutoff
    THEN
        RAISE EXCEPTION 'PERIOD_LOCKED'
            USING ERRCODE = '23514';
    END IF;

    -- Both regular postings and reversals use the same validation.
    IF v_entries IS NULL
       OR jsonb_typeof(v_entries) IS DISTINCT FROM 'array'
    THEN
        RAISE EXCEPTION 'INVALID_ENTRIES'
            USING ERRCODE = '23514';
    END IF;

    v_entry_count := jsonb_array_length(v_entries);

    IF v_entry_count NOT BETWEEN 2 AND 64 THEN
        IF p_original_id IS NOT NULL THEN
            RAISE EXCEPTION 'INVALID_ORIGINAL_TRANSACTION'
                USING ERRCODE = '23514';
        END IF;

        RAISE EXCEPTION 'INVALID_ENTRY_COUNT: %', v_entry_count
            USING ERRCODE = '23514';
    END IF;

    IF EXISTS (
        SELECT 1
        FROM jsonb_to_recordset(v_entries) AS e(
            account_id UUID,
            side TEXT,
            amount BIGINT
        )
        WHERE e.account_id IS NULL
           OR e.side IS NULL
           OR e.side NOT IN ('D', 'C')
           OR e.amount IS NULL
           OR e.amount NOT BETWEEN 1 AND 1000000000000
    ) THEN
        RAISE EXCEPTION 'INVALID_ENTRY_DATA'
            USING ERRCODE = '23514';
    END IF;

    SELECT
        COALESCE(
            SUM(e.amount::numeric)
                FILTER (WHERE e.side = 'D'),
            0
        ),
        COALESCE(
            SUM(e.amount::numeric)
                FILTER (WHERE e.side = 'C'),
            0
        )
    INTO v_debit_sum, v_credit_sum
    FROM jsonb_to_recordset(v_entries) AS e(
        account_id UUID,
        side TEXT,
        amount BIGINT
    );

    IF v_debit_sum <> v_credit_sum THEN
        IF p_original_id IS NOT NULL THEN
            RAISE EXCEPTION 'INVALID_ORIGINAL_TRANSACTION'
                USING ERRCODE = '23514';
        END IF;

        RAISE EXCEPTION 'UNBALANCED_TRANSACTION'
            USING ERRCODE = '23514';
    END IF;

    -- Aggregate repeated accounts before acquiring balance locks.
    -- Every operation locks balances in ascending UUID order.
    FOR v_delta IN
        SELECT
            e.account_id,
            COALESCE(
                SUM(e.amount) FILTER (WHERE e.side = 'D'),
                0
            )::bigint AS debit_change,
            COALESCE(
                SUM(e.amount) FILTER (WHERE e.side = 'C'),
                0
            )::bigint AS credit_change
        FROM jsonb_to_recordset(v_entries) AS e(
            account_id UUID,
            side TEXT,
            amount BIGINT
        )
        GROUP BY e.account_id
        ORDER BY e.account_id
    LOOP
        SELECT
            a.code,
            a.status
        INTO
            v_account_code,
            v_account_status
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

        -- Apply cumulative changes exactly once.
        -- Table constraints enforce balance and reservation policies.
        UPDATE ledger.account_balances AS b
        SET
            debit_total = b.debit_total + v_delta.debit_change,
            credit_total = b.credit_total + v_delta.credit_change,
            version = b.version + 1,
            updated_at = NOW()
        WHERE b.ledger_id = p_ledger_id
          AND b.account_id = v_delta.account_id
        RETURNING
            b.debit_total,
            b.credit_total,
            b.reserved_total,
            b.normal_side,
            b.version
        INTO v_balance;

        -- Build snapshots from the actual updated database state.
        IF p_server_response THEN
            v_posted := CASE
                WHEN v_balance.normal_side = 'D'
                    THEN v_balance.debit_total
                         - v_balance.credit_total
                ELSE v_balance.credit_total
                     - v_balance.debit_total
            END;

            v_available :=
                v_posted - v_balance.reserved_total;

            v_balances := v_balances || jsonb_build_array(
                jsonb_build_object(
                    'account_id', v_delta.account_id,
                    'account_code', v_account_code,
                    'posted_total', v_posted::text,
                    'reserved_total',
                        v_balance.reserved_total::text,
                    'available_total', v_available::text,
                    'version', v_balance.version
                )
            );
        END IF;
    END LOOP;

    IF p_server_response THEN
        v_snapshot := jsonb_build_object(
            'status', 'posted',
            'transaction_id', p_transaction_id,
            'balances_after', v_balances
        );

        IF p_original_id IS NOT NULL THEN
            v_snapshot := v_snapshot || jsonb_build_object(
                'reverses_transaction', p_original_id
            );
        END IF;

        v_response := convert_to(
            v_snapshot::text,
            'UTF8'
        );
    ELSE
        -- Preserve the existing trusted posting contract.
        v_response := p_response_body;
    END IF;

    -- Persist the final response and reversal relationship at INSERT.
    -- The transaction row remains immutable after creation.
    INSERT INTO ledger.transactions (
        ledger_id,
        id,
        route_scope,
        idempotency_key,
        request_hash,
        response_snapshot,
        response_body,
        reverses_transaction,
        effective_at
    )
    VALUES (
        p_ledger_id,
        p_transaction_id,
        p_route_scope,
        p_idempotency_key,
        p_request_hash,
        convert_from(v_response, 'UTF8')::jsonb,
        v_response,
        p_original_id,
        p_effective_at
    )
    ON CONFLICT ON CONSTRAINT transactions_idempotency_unique
    DO NOTHING;

    -- A conflicting writer outside the shared lock protocol
    -- must never leave provisional balance changes behind.
    IF NOT FOUND THEN
        RAISE EXCEPTION 'IDEMPOTENCY_RETRY_REQUIRED'
            USING ERRCODE = '40001';
    END IF;

    -- Preserve the input journal order.
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
        p_transaction_id,
        (e.value->>'account_id')::uuid,
        (e.value->>'side')::char(1),
        (e.value->>'amount')::bigint,
        p_effective_at
    FROM jsonb_array_elements(v_entries)
        WITH ORDINALITY AS e(value, position)
    ORDER BY e.position;

    -- Deferred balance and reversal constraints run at COMMIT.
    RETURN QUERY
    SELECT
        p_transaction_id,
        FALSE,
        v_response;
END;
$$;

ALTER FUNCTION ledger.execute_posting(
    UUID, UUID, TEXT, TEXT, BYTEA,
    BYTEA, TIMESTAMPTZ, JSONB, UUID, BOOLEAN
)
OWNER TO ledger_migrator;

REVOKE ALL
ON FUNCTION ledger.execute_posting(
    UUID, UUID, TEXT, TEXT, BYTEA,
    BYTEA, TIMESTAMPTZ, JSONB, UUID, BOOLEAN
)
FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;
