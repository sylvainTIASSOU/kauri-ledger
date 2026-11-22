
-- Track transactions that have passed deferred accounting validation.
-- Runtime roles must not receive direct INSERT, UPDATE, DELETE
-- or TRUNCATE privileges on this table.
CREATE TABLE ledger.transaction_seals (
    ledger_id UUID NOT NULL,
    transaction_id UUID NOT NULL,
    sealed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT transaction_seals_pk
        PRIMARY KEY (ledger_id, transaction_id),

    CONSTRAINT transaction_seals_transaction_fk
        FOREIGN KEY (ledger_id, transaction_id)
        REFERENCES ledger.transactions (ledger_id, id)
        ON DELETE RESTRICT
);

-- Reject modifications to append-only accounting tables.
CREATE FUNCTION ledger.reject_ledger_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION
        'APPEND_ONLY: % on %.% is forbidden',
        TG_OP, TG_TABLE_SCHEMA, TG_TABLE_NAME
        USING ERRCODE = '23514';

    RETURN NULL;
END;
$$;

-- Serialize entry insertion against transaction finalization.
-- A committed transaction must already have a seal.
CREATE FUNCTION ledger.guard_entry_insertion()
RETURNS trigger
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    -- Lock the parent row before checking its seal.
    -- Concurrent insertions wait for the posting transaction
    -- to commit or roll back.
    PERFORM 1
    FROM ledger.transactions
    WHERE ledger_id = NEW.ledger_id
      AND id = NEW.transaction_id
      AND effective_at = NEW.effective_at
    FOR UPDATE;

    IF NOT FOUND THEN
        RAISE EXCEPTION
            'TRANSACTION_NOT_FOUND: %',
            NEW.transaction_id
            USING ERRCODE = '23503';
    END IF;

    IF EXISTS (
        SELECT 1
        FROM ledger.transaction_seals
        WHERE ledger_id = NEW.ledger_id
          AND transaction_id = NEW.transaction_id
    ) THEN
        RAISE EXCEPTION
            'TRANSACTION_SEALED: %',
            NEW.transaction_id
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

-- Validate the complete transaction, including transactions
-- with no entries, before it can be committed.
CREATE FUNCTION ledger.assert_transaction_balanced()
RETURNS trigger
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    entry_count BIGINT;
    imbalance NUMERIC;
BEGIN
    SELECT
        COUNT(*),
        COALESCE(
            SUM(
                CASE
                    WHEN side = 'D' THEN amount
                    ELSE -amount
                END
            ),
            0
        )
    INTO entry_count, imbalance
    FROM ledger.entries
    WHERE ledger_id = NEW.ledger_id
      AND transaction_id = NEW.id;

    IF entry_count < 2 THEN
        RAISE EXCEPTION
            'INSUFFICIENT_ENTRIES: transaction % has % entries',
            NEW.id, entry_count
            USING ERRCODE = '23514';
    END IF;

    IF entry_count > 64 THEN
        RAISE EXCEPTION
            'TOO_MANY_ENTRIES: transaction % has % entries',
            NEW.id, entry_count
            USING ERRCODE = '23514';
    END IF;

    IF imbalance <> 0 THEN
        RAISE EXCEPTION
            'UNBALANCED_TRANSACTION: transaction %',
            NEW.id
            USING ERRCODE = '23514';
    END IF;

    -- The seal is inserted in the same SQL transaction.
    -- A failed validation rolls back both the posting and seal.
    INSERT INTO ledger.transaction_seals (
        ledger_id,
        transaction_id
    )
    VALUES (
        NEW.ledger_id,
        NEW.id
    );

    RETURN NULL;
END;
$$;

-- Run final validation at COMMIT by default.
CREATE CONSTRAINT TRIGGER transactions_balanced
AFTER INSERT ON ledger.transactions
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION ledger.assert_transaction_balanced();

CREATE TRIGGER entries_guard_insert
BEFORE INSERT ON ledger.entries
FOR EACH ROW
EXECUTE FUNCTION ledger.guard_entry_insertion();

-- Prevent updates and deletes after insertion.
CREATE TRIGGER transactions_immutable
BEFORE UPDATE OR DELETE ON ledger.transactions
FOR EACH ROW
EXECUTE FUNCTION ledger.reject_ledger_mutation();

CREATE TRIGGER entries_immutable
BEFORE UPDATE OR DELETE ON ledger.entries
FOR EACH ROW
EXECUTE FUNCTION ledger.reject_ledger_mutation();

CREATE TRIGGER transaction_seals_immutable
BEFORE UPDATE OR DELETE ON ledger.transaction_seals
FOR EACH ROW
EXECUTE FUNCTION ledger.reject_ledger_mutation();

-- Row-level triggers do not protect against TRUNCATE.
CREATE TRIGGER transactions_no_truncate
BEFORE TRUNCATE ON ledger.transactions
FOR EACH STATEMENT
EXECUTE FUNCTION ledger.reject_ledger_mutation();

CREATE TRIGGER entries_no_truncate
BEFORE TRUNCATE ON ledger.entries
FOR EACH STATEMENT
EXECUTE FUNCTION ledger.reject_ledger_mutation();

CREATE TRIGGER transaction_seals_no_truncate
BEFORE TRUNCATE ON ledger.transaction_seals
FOR EACH STATEMENT
EXECUTE FUNCTION ledger.reject_ledger_mutation();

-- Restrict direct function execution.
-- Migration owners retain the privileges needed by triggers.
REVOKE ALL
ON FUNCTION ledger.guard_entry_insertion()
FROM PUBLIC;

REVOKE ALL
ON FUNCTION ledger.assert_transaction_balanced()
FROM PUBLIC;

REVOKE ALL
ON FUNCTION ledger.reject_ledger_mutation()
FROM PUBLIC;
