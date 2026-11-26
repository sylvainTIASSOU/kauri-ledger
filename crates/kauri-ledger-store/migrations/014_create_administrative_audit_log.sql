
-- Record administrative security events in an append-only audit log.
-- API key secrets and hashes must never appear in these events.

CREATE TABLE ledger.audit_log (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,

    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    target_type TEXT NOT NULL,
    target_id UUID NOT NULL,
    ledger_id UUID NOT NULL,

    recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT audit_log_actor_valid
        CHECK (
            char_length(actor) BETWEEN 1 AND 200
            AND char_length(btrim(actor)) > 0
        ),

    CONSTRAINT audit_log_action_valid
        CHECK (
            action IN (
                'api_key_created',
                'api_key_revoked'
            )
        ),

    CONSTRAINT audit_log_target_type_valid
        CHECK (target_type = 'api_key'),

    CONSTRAINT audit_log_ledger_fk
        FOREIGN KEY (ledger_id)
        REFERENCES ledger.ledgers (id)
        ON DELETE RESTRICT
);

CREATE INDEX audit_log_target_lookup
    ON ledger.audit_log (
        target_type,
        target_id,
        recorded_at DESC
    );

CREATE INDEX audit_log_ledger_timeline
    ON ledger.audit_log (
        ledger_id,
        recorded_at DESC
    );

-- The trusted migration role owns the audit log.
ALTER TABLE ledger.audit_log OWNER TO ledger_migrator;

-- Tenant-facing roles cannot read or alter administrative events.
REVOKE ALL ON ledger.audit_log
FROM PUBLIC, ledger_app, ledger_admin, ledger_readonly;

-- Reuse the existing append-only protection function.
CREATE TRIGGER audit_log_immutable
BEFORE UPDATE OR DELETE ON ledger.audit_log
FOR EACH ROW
EXECUTE FUNCTION ledger.reject_ledger_mutation();

CREATE TRIGGER audit_log_no_truncate
BEFORE TRUNCATE ON ledger.audit_log
FOR EACH STATEMENT
EXECUTE FUNCTION ledger.reject_ledger_mutation();
