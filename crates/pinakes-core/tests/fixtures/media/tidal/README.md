TIDAL's own records that `tests/tidal.rs` serves for its fake TIDAL API (the `tidal` also-on source's own recordings, removed with it):

| File | Answers |
| --- | --- |
| `tidal-albums-199574241-495402659.json` | `GET /v2/albums` with `filter[id]` 199574241 and 495402659: HYDRA 3D and DRACULA, with their barcodes |
| `tidal-tracks-199574242.json` | `GET /v2/tracks` with `filter[id]` 199574242: HYDRA 3D's title track, with its ISRC; `/v2/tracks/199574242` is its first item |
