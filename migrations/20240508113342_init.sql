CREATE TABLE IF NOT EXISTS rooms
(
    id         INTEGER PRIMARY KEY NOT NULL,
    admin_code TEXT                NOT NULL,
    name       TEXT                NOT NULL,
    options    TEXT                NOT NULL,
    status     INTEGER             NOT NULL DEFAULT 0, -- 0 = waiting, 1 = started, 2 = ended
    created_at TIMESTAMP           NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX idx_rooms_admin_code ON rooms (admin_code);

CREATE TABLE IF NOT EXISTS voters
(
    id         INTEGER PRIMARY KEY NOT NULL,
    voter_code TEXT                NOT NULL,
    options    TEXT                NULL,
    approved   BOOLEAN             NOT NULL DEFAULT 0,
    room_id    INTEGER             NOT NULL REFERENCES rooms(id),
    created_at TIMESTAMP           NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX idx_voters_voter_code ON voters (voter_code);
