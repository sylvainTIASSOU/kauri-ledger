
-- Create an authorized, atomic reversal of a posted transaction.
-- Original journal entries are never updated or deleted.
-- Reversal entries are derived exclusively from the stored journal.

CREATE FUNCTION ledger.authorized_reverse_transaction(
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
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_route_scope TEXT;

    v_existing_id UUID;
    v_existing_hash BYTEA;
    v_existing_body BYTEA;

    v_original_id UUID;
    v_entry_count BIGINT;
    v_imbalance NUMERIC;

    v_delta RECORD;
    v_balance RECORD;

    v_debit BIGINT;
    v_credit BIGINT;
    v_posted BIGINT;
    v_available BIGINT;

    v_balances JSONB := '[]'::jsonb;
    v_response BYTEA;
BEGIN
    -- Authorization is checked inside the accounting operation.
    PERFORM ledger.require_api_access(
        p_prefix,
        p_key_hash,
        p_ledger_id,
        'write'
    );

    v_route_scope :=
        'POST /v1/ledgers/'
        || p_ledger_id::text
        || '/transactions/'
        || p_original_transaction_id::text
        || '/reverse';

    IF p_original_transaction_id IS NULL
       OR p_reversal_transaction_id IS NULL
       OR p_original_transaction_id = p_reversal_transaction_id
       OR p_effective_at IS NULL
       OR p_idempotency_key IS NULL
       OR char_length(p_idempotency_key) NOT BETWEEN 8 AND 200
       OR p_request_hash IS NULL
       OR octet_length(p_request_hash) <> 32
    THEN
        RAISE EXCEPTION 'INVALID_REVERSAL_REQUEST'
            USING ERRCODE = '23514';
    END IF;

    -- Fast path: return the exact previously committed response.
    -- A replay must remain possible after a ledger freeze.
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
      AND t.route_scope = v_route_scope
      AND t.idempotency_key = p_idempotency_key;

    IF FOUND THEN
        IF v_existing_hash IS DISTINCT FROM p_request_hash THEN
            RAISE EXCEPTION 'IDEMPOTENCY_KEY_REUSED'
                USING ERRCODE = '23514';
        END IF;

        RETURN QUERY
        SELECT v_existing_id, TRUE, v_existing_body;

        RETURN;
    END IF;

    -- Follow the established lock order:
    -- ledger -> original transaction -> account balances.
    PERFORM 1
    FROM ledger.ledgers AS l
    WHERE l.id = p_ledger_id
    FOR SHARE;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'LEDGER_NOT_FOUND'
            USING ERRCODE = 'P0002';
    END IF;

    -- Serialize concurrent reversal attempts for this original.
    SELECT t.id
    INTO v_original_id
    FROM ledger.transactions AS t
    WHERE t.ledger_id = p_ledger_id
      AND t.id = p_original_transaction_id
    FOR UPDATE;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'TRANSACTION_NOT_FOUND'
            USING ERRCODE = 'P0002';
    END IF;

    -- Another request may have completed while we waited for
    -- the original transaction's row lock.
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
      AND t.route_scope = v_route_scope
      AND t.idempotency_key = p_idempotency_key;

    IF FOUND THEN
        IF v_existing_hash IS DISTINCT FROM p_request_hash THEN
            RAISE EXCEPTION 'IDEMPOTENCY_KEY_REUSED'
                USING ERRCODE = '23514';
        END IF;

        RETURN QUERY
        SELECT v_existing_id, TRUE, v_existing_body;

        RETURN;
    END IF;

    -- A different idempotency key must not create another reversal.
    IF EXISTS (
        SELECT 1
        FROM ledger.transactions AS t
        WHERE t.ledger_id = p_ledger_id
          AND t.reverses_transaction = p_original_transaction_id
    ) THEN
        RAISE EXCEPTION 'ALREADY_REVERSED'
            USING ERRCODE = '23514';
    END IF;

    -- A reversal must reference a validated, sealed transaction.
    IF NOT EXISTS (
        SELECT 1
        FROM ledger.transaction_seals AS s
        WHERE s.ledger_id = p_ledger_id
          AND s.transaction_id = p_original_transaction_id
    ) THEN
        RAISE EXCEPTION 'TRANSACTION_NOT_SEALED'
            USING ERRCODE = '23514';
    END IF;

    -- Independently verify the source journal before reversing it.
    SELECT
        COUNT(*),
        COALESCE(
            SUM(
                CASE
                    WHEN e.side = 'D'
                        THEN e.amount::numeric
                    ELSE -e.amount::numeric
                END
            ),
            0
        )
    INTO v_entry_count, v_imbalance
    FROM ledger.entries AS e
    WHERE e.ledger_id = p_ledger_id
      AND e.transaction_id = p_original_transaction_id;

    IF v_entry_count NOT BETWEEN 2 AND 64
       OR v_imbalance <> 0
    THEN
        RAISE EXCEPTION 'INVALID_ORIGINAL_TRANSACTION'
            USING ERRCODE = '23514';
    END IF;

    -- Reverse every original entry:
    -- original debit becomes new credit;
    -- original credit becomes new debit.
    --
    -- Aggregate repeated accounts, then lock them in UUID order.
    FOR v_delta IN
        SELECT
            e.account_id,
            SUM(
                CASE
                    WHEN e.side = 'C' THEN e.amount
                    ELSE 0
                END
            )::BIGINT AS debit_change,
            SUM(
                CASE
                    WHEN e.side = 'D' THEN e.amount
                    ELSE 0
                END
            )::BIGINT AS credit_change
        FROM ledger.entries AS e
        WHERE e.ledger_id = p_ledger_id
          AND e.transaction_id = p_original_transaction_id
        GROUP BY e.account_id
        ORDER BY e.account_id
    LOOP
        SELECT
            b.debit_total,
            b.credit_total,
            b.reserved_total,
            b.normal_side,
            b.version,
            a.code,
            a.status
        INTO v_balance
        FROM ledger.account_balances AS b
        JOIN ledger.accounts AS a
          ON a.ledger_id = b.ledger_id
         AND a.id = b.account_id
        WHERE b.ledger_id = p_ledger_id
          AND b.account_id = v_delta.account_id
        FOR UPDATE OF b;

        IF NOT FOUND THEN
            RAISE EXCEPTION 'ACCOUNT_BALANCE_NOT_FOUND'
                USING ERRCODE = '23503';
        END IF;

        IF v_balance.status <> 'active' THEN
            RAISE EXCEPTION 'ACCOUNT_CLOSED'
                USING ERRCODE = '23514';
        END IF;

        v_debit :=
            v_balance.debit_total + v_delta.debit_change;

        v_credit :=
            v_balance.credit_total + v_delta.credit_change;

        v_posted := CASE
            WHEN v_balance.normal_side = 'D'
                THEN v_debit - v_credit
            ELSE v_credit - v_debit
        END;

        v_available :=
            v_posted - v_balance.reserved_total;

        -- Existing balance constraints enforce overdraft policy,
        -- reservation policy and maximum cumulative totals.
        UPDATE ledger.account_balances AS b
        SET
            debit_total = v_debit,
            credit_total = v_credit,
            version = b.version + 1,
            updated_at = NOW()
        WHERE b.ledger_id = p_ledger_id
          AND b.account_id = v_delta.account_id;

        v_balances := v_balances || jsonb_build_array(
            jsonb_build_object(
                'account_id', v_delta.account_id,
                'account_code', v_balance.code,
                'posted_total', v_posted::text,
                'reserved_total',
                    v_balance.reserved_total::text,
                'available_total', v_available::text,
                'version', v_balance.version + 1
            )
        );
    END LOOP;

    -- Build the final response before inserting the transaction.
    -- These exact bytes will be returned on every idempotent replay.
    v_response := convert_to(
        jsonb_build_object(
            'status', 'posted',
            'transaction_id', p_reversal_transaction_id,
            'reverses_transaction',
                p_original_transaction_id,
            'balances_after', v_balances
        )::text,
        'UTF8'
    );

    -- The link to the original must exist at INSERT time.
    -- The transactions table is append-only.
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
        p_reversal_transaction_id,
        v_route_scope,
        p_idempotency_key,
        p_request_hash,
        convert_from(v_response, 'UTF8')::jsonb,
        v_response,
        p_original_transaction_id,
        p_effective_at
    );

    -- Preserve the order and amounts of the source journal.
    -- Only the debit/credit direction changes.
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
        p_reversal_transaction_id,
        e.account_id,
        CASE
            WHEN e.side = 'D' THEN 'C'
            ELSE 'D'
        END,
        e.amount,
        p_effective_at
    FROM ledger.entries AS e
    WHERE e.ledger_id = p_ledger_id
      AND e.transaction_id = p_original_transaction_id
    ORDER BY e.id;

    -- Deferred transaction validation inserts the new seal
    -- when the surrounding transaction commits successfully.
    RETURN QUERY
    SELECT p_reversal_transaction_id, FALSE, v_response;
END;
$$;

ALTER FUNCTION ledger.authorized_reverse_transaction(
    TEXT, BYTEA, UUID, UUID, UUID, TEXT, BYTEA, TIMESTAMPTZ
)
OWNER TO ledger_migrator;

REVOKE ALL
ON FUNCTION ledger.authorized_reverse_transaction(
    TEXT, BYTEA, UUID, UUID, UUID, TEXT, BYTEA, TIMESTAMPTZ
)
FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;

GRANT EXECUTE
ON FUNCTION ledger.authorized_reverse_transaction(
    TEXT, BYTEA, UUID, UUID, UUID, TEXT, BYTEA, TIMESTAMPTZ
)
TO ledger_app;
