"""Create the database schema. Runs once per deploy, before the backend starts."""

import os

import psycopg

with psycopg.connect(os.environ["DATABASE_URL"]) as conn:
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS hits (
            id serial PRIMARY KEY,
            at timestamptz NOT NULL DEFAULT now()
        )
        """
    )
print("migration done")
