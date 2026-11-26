
-- Enforce exact reversal semantics independently of the posting path.
-- Every reversal must contain the same accounts, amounts, currencies
-- and entry multiplicities as its original, with opposite directions.

CREATE FUNCTION ledger.reversal_matches_original(
    p_ledger_id UUID,
    p_original_id UUID,
    p_reversal_id UUID
)
RETURNS BOOLEAN
LANGUAGE sql
STABLE
SECURITY INVOKER
SET search_path = pg_catalog, pg_temp
AS $$
    WITH expected_entries AS (
        SELECT
            e.account_id,
            CASE
                WHEN e.side = 'D' THEN 'C'
                ELSE 'D'
            END AS side,
            e.amount,
            e.currency::text AS currency
        FROM ledger.entries AS e
        WHERE e.ledger_id = p_ledger_id
          AND e.transaction_id = p_original_id
    ),
    actual_entries AS (
        SELECT
            e.account_id,
            e.side::text AS side,
            e.amount,
            e.currency::text AS currency
        FROM ledger.entries AS e
        WHERE e.ledger_id = p_ledger_id
          AND e.transaction_id = p_reversal_id
    ),
    differences AS (
        (
            SELECT *
            FROM expected_entries

            EXCEPT ALL

            SELECT *
            FROM actual_entries
        )

        UNION ALL

        (
            SELECT *
            FROM actual_entries

            EXCEPT ALL

            SELECT *
            FROM expected_entries
        )
    )
    SELECT
        p_original_id <> p_reversal_id

        AND EXISTS (
            SELECT 1
            FROM ledger.transactions AS t
            WHERE t.ledger_id = p_ledger_id
              AND t.id = p_original_id
        )

        AND EXISTS (
            SELECT 1
            FROM ledger.transaction_seals AS s
            WHERE s.ledger_id = p_ledger_id
              AND s.transaction_id = p_original_id
        )

        AND NOT EXISTS (
            SELECT 1
            FROM differences
        );
$$;

ALTER FUNCTION ledger.reversal_matches_original(
    UUID, UUID, UUID
)
OWNER TO ledger_migrator;

REVOKE ALL
ON FUNCTION ledger.reversal_matches_original(
    UUID, UUID, UUID
)
FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;

-- The trusted audit role may verify reversal relationships.
-- SECURITY INVOKER preserves its existing table permissions.
GRANT EXECUTE
ON FUNCTION ledger.reversal_matches_original(
    UUID, UUID, UUID
)
TO ledger_readonly;


-- Validate the complete reversal before the transaction commits.
CREATE FUNCTION ledger.assert_exact_reversal()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF ledger.reversal_matches_original(
        NEW.ledger_id,
        NEW.reverses_transaction,
        NEW.id
    ) IS DISTINCT FROM TRUE THEN
        RAISE EXCEPTION
            'INVALID_REVERSAL_ENTRIES: transaction %',
            NEW.id
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END;
$$;

ALTER FUNCTION ledger.assert_exact_reversal()
OWNER TO ledger_migrator;

REVOKE ALL
ON FUNCTION ledger.assert_exact_reversal()
FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;


-- Run after all journal entries have been inserted.
-- Ordinary transactions do not need this additional check.
CREATE CONSTRAINT TRIGGER transactions_reversal_exact
AFTER INSERT ON ledger.transactions
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
WHEN (NEW.reverses_transaction IS NOT NULL)
EXECUTE FUNCTION ledger.assert_exact_reversal();


-- Validate historical reversals before accepting the migration.
-- Never silently repair or rewrite immutable financial records.
DO $$
DECLARE
    v_invalid RECORD;
BEGIN
    SELECT
        t.ledger_id,
        t.id
    INTO v_invalid
    FROM ledger.transactions AS t
    WHERE t.reverses_transaction IS NOT NULL
      AND ledger.reversal_matches_original(
          t.ledger_id,
          t.reverses_transaction,
          t.id
      ) IS DISTINCT FROM TRUE
    LIMIT 1;

    IF FOUND THEN
        RAISE EXCEPTION
            'INVALID_EXISTING_REVERSAL: ledger %, transaction %',
            v_invalid.ledger_id,
            v_invalid.id
            USING ERRCODE = '23514';
    END IF;
END;
$$;
