
-- Provision trusted PostgreSQL group roles.
-- Run this script with an authorized DBA connection.

DO $$
DECLARE
    role_name TEXT;
BEGIN
    FOREACH role_name IN ARRAY ARRAY[
        'ledger_migrator',
        'ledger_app',
        'ledger_admin',
        'ledger_readonly'
    ]
    LOOP
        IF NOT EXISTS (
            SELECT 1
            FROM pg_roles
            WHERE rolname = role_name
        ) THEN
            EXECUTE format(
                'CREATE ROLE %I NOLOGIN NOINHERIT '
                || 'NOSUPERUSER NOCREATEDB NOCREATEROLE '
                || 'NOREPLICATION NOBYPASSRLS',
                role_name
            );
        END IF;

        IF EXISTS (
            SELECT 1
            FROM pg_roles
            WHERE rolname = role_name
              AND (
                  rolcanlogin
                  OR rolinherit
                  OR rolsuper
                  OR rolcreatedb
                  OR rolcreaterole
                  OR rolreplication
                  OR rolbypassrls
              )
        ) THEN
            RAISE EXCEPTION
                'UNSAFE_ROLE_CONFIGURATION: %',
                role_name;
        END IF;
    END LOOP;
END;
$$;
