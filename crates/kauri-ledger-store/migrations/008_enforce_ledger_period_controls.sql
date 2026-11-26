
-- Store the latest closed accounting period for each ledger.
-- The absence of a row means that no period has been closed.
CREATE TABLE ledger.period_locks (
    ledger_id UUID PRIMARY KEY,

    locked_until TIMESTAMPTZ NOT NULL,

    CONSTRAINT period_locks_ledger_fk
        FOREIGN KEY (ledger_id)
        REFERENCES ledger.ledgers (id)
        ON DELETE RESTRICT
);

-- Validate the ledger state and accounting period before
-- inserting a new transaction.
CREATE FUNCTION ledger.assert_period_open()
RETURNS trigger
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    current_status TEXT;
    period_cutoff TIMESTAMPTZ;
BEGIN
    -- This shared row lock remains held until transaction end.
    -- It prevents a concurrent freeze or period closure
    -- from overtaking an in-flight posting.
    SELECT status
    INTO current_status
    FROM ledger.ledgers
    WHERE id = NEW.ledger_id
    FOR SHARE;

    IF NOT FOUND THEN
        RAISE EXCEPTION
            'LEDGER_NOT_FOUND: ledger % does not exist',
            NEW.ledger_id
            USING ERRCODE = '23503';
    END IF;

    IF current_status = 'frozen' THEN
        RAISE EXCEPTION
            'LEDGER_FROZEN: ledger % is frozen',
            NEW.ledger_id
            USING ERRCODE = '23514';
    END IF;

    SELECT locked_until
    INTO period_cutoff
    FROM ledger.period_locks
    WHERE ledger_id = NEW.ledger_id;

    IF period_cutoff IS NOT NULL
       AND NEW.effective_at <= period_cutoff THEN
        RAISE EXCEPTION
            'PERIOD_LOCKED: effective date is not after %',
            period_cutoff
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER transactions_period_open
BEFORE INSERT ON ledger.transactions
FOR EACH ROW
EXECUTE FUNCTION ledger.assert_period_open();

-- Advance a period closure while holding an exclusive
-- lock on the parent ledger row.
--
-- Authorized administrative callers must use this function
-- rather than modifying period_locks directly.
CREATE FUNCTION ledger.close_period(
    p_ledger_id UUID,
    p_locked_until TIMESTAMPTZ
)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF p_locked_until IS NULL THEN
        RAISE EXCEPTION
            'INVALID_PERIOD_CUTOFF: cutoff must not be NULL'
            USING ERRCODE = '23514';
    END IF;

    -- Acquire the parent lock before accessing period_locks.
    -- This is the same ordering used by posting operations.
    PERFORM 1
    FROM ledger.ledgers
    WHERE id = p_ledger_id
    FOR UPDATE;

    IF NOT FOUND THEN
        RAISE EXCEPTION
            'LEDGER_NOT_FOUND: ledger % does not exist',
            p_ledger_id
            USING ERRCODE = '23503';
    END IF;

    -- Period closures must never move backward.
    INSERT INTO ledger.period_locks (
        ledger_id,
        locked_until
    )
    VALUES (
        p_ledger_id,
        p_locked_until
    )
    ON CONFLICT (ledger_id)
    DO UPDATE
        SET locked_until = EXCLUDED.locked_until
        WHERE period_locks.locked_until
              <= EXCLUDED.locked_until;

    IF NOT FOUND THEN
        RAISE EXCEPTION
            'PERIOD_LOCK_REWIND: cutoff cannot move backward'
            USING ERRCODE = '23514';
    END IF;
END;
$$;

-- Administrative functions are not public APIs.
-- Explicit administrative grants will be defined in H2.4.
REVOKE ALL
ON FUNCTION ledger.assert_period_open()
FROM PUBLIC;

REVOKE ALL
ON FUNCTION ledger.close_period(UUID, TIMESTAMPTZ)
FROM PUBLIC;
