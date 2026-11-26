
-- Atomically create a ledger account and its initial balance.
-- Only trusted database roles may execute this function.

CREATE FUNCTION ledger.create_account(
    p_ledger_id UUID,
    p_account_id UUID,
    p_code TEXT,
    p_kind TEXT,
    p_name TEXT,
    p_allow_negative BOOLEAN
)
RETURNS UUID
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    v_ledger_status TEXT;
    v_normal_side CHAR(1);
BEGIN
    -- Serialize account creation against ledger freezes.
    SELECT status
    INTO v_ledger_status
    FROM ledger.ledgers
    WHERE id = p_ledger_id
    FOR SHARE;

    IF NOT FOUND THEN
        RAISE EXCEPTION
            'LEDGER_NOT_FOUND: ledger % does not exist',
            p_ledger_id
            USING ERRCODE = '23503';
    END IF;

    IF v_ledger_status <> 'active' THEN
        RAISE EXCEPTION
            'LEDGER_FROZEN: ledger % is frozen',
            p_ledger_id
            USING ERRCODE = '23514';
    END IF;

    -- Derive the natural accounting side from the account kind.
    v_normal_side := CASE p_kind
        WHEN 'asset' THEN 'D'
        WHEN 'expense' THEN 'D'
        WHEN 'liability' THEN 'C'
        WHEN 'equity' THEN 'C'
        WHEN 'revenue' THEN 'C'
        ELSE NULL
    END;

    IF v_normal_side IS NULL THEN
        RAISE EXCEPTION
            'INVALID_ACCOUNT_KIND: %',
            p_kind
            USING ERRCODE = '23514';
    END IF;

    -- Existing table constraints validate the account code,
    -- name, identifiers and accounting properties.
    INSERT INTO ledger.accounts (
        ledger_id,
        id,
        code,
        kind,
        normal_side,
        allow_negative,
        name
    )
    VALUES (
        p_ledger_id,
        p_account_id,
        p_code,
        p_kind,
        v_normal_side,
        p_allow_negative,
        p_name
    );

    -- This insertion is part of the same SQL transaction.
    INSERT INTO ledger.account_balances (
        ledger_id,
        account_id,
        normal_side,
        allow_negative,
        debit_total,
        credit_total,
        reserved_total,
        version
    )
    VALUES (
        p_ledger_id,
        p_account_id,
        v_normal_side,
        p_allow_negative,
        0,
        0,
        0,
        0
    );

    RETURN p_account_id;
END;
$$;

-- Execute with the privileges of the dedicated migration role.
ALTER FUNCTION ledger.create_account(
    UUID, UUID, TEXT, TEXT, TEXT, BOOLEAN
)
OWNER TO ledger_migrator;

-- Never expose this privileged function implicitly.
REVOKE ALL
ON FUNCTION ledger.create_account(
    UUID, UUID, TEXT, TEXT, TEXT, BOOLEAN
)
FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;
