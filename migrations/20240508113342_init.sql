CREATE TABLE IF NOT EXISTS rooms
(
    id         INTEGER PRIMARY KEY NOT NULL,
    vid        TEXT                NOT NULL UNIQUE,
    admin_code TEXT                NOT NULL,
    name       TEXT                NOT NULL,
    options    TEXT                NOT NULL,
    started    BOOLEAN             NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP           NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX idx_rooms_vid ON rooms (vid);

CREATE TABLE IF NOT EXISTS voters
(
    id         INTEGER PRIMARY KEY NOT NULL,
    vid        TEXT                NOT NULL UNIQUE,
    approved   BOOLEAN             NOT NULL DEFAULT 0,
    room_id    INTEGER             NOT NULL REFERENCES rooms(id),
    created_at TIMESTAMP           NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX idx_voters_vid ON voters (vid);
