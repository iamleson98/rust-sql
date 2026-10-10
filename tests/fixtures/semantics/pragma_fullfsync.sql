-- PRAGMA fullfsync / checkpoint_fullfsync: SQLite's defaults (0), its
-- boolean spellings, and the read-back. (On Apple platforms the setting
-- picks F_FULLFSYNC over a plain fsync for commits / checkpoints.)
PRAGMA fullfsync;
PRAGMA checkpoint_fullfsync;
PRAGMA fullfsync = ON;
PRAGMA fullfsync;
PRAGMA fullfsync = no;
PRAGMA fullfsync;
PRAGMA fullfsync = 1;
PRAGMA fullfsync;
PRAGMA checkpoint_fullfsync = true;
PRAGMA checkpoint_fullfsync;
PRAGMA checkpoint_fullfsync = 0;
PRAGMA checkpoint_fullfsync;
