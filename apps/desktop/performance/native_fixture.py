"""Seed only the isolated native performance app; never read user library/tokens."""
import argparse
import json
import os
from pathlib import Path
import wave

parser = argparse.ArgumentParser()
parser.add_argument('--data-dir', type=Path, required=True)
parser.add_argument('--media-dir', type=Path, required=True)
parser.add_argument('--count', type=int, default=4000)
args = parser.parse_args()
assert args.data_dir.name == 'com.rianjs.retune.performance'
assert args.count > 0
args.data_dir.mkdir(parents=True, exist_ok=True)
assert not (args.data_dir / 'library.json').exists(), 'Refuse to replace an existing library'
args.media_dir.mkdir(parents=True, exist_ok=True)
audio = args.media_dir / 'silence.wav'
with wave.open(str(audio), 'wb') as output:
    output.setparams((1, 2, 8000, 0, 'NONE', 'not compressed'))
    for _ in range(3600):
        output.writeframes(b'\0' * 16000)
tracks = []
for index in range(args.count):
    media = args.media_dir / f'{index:05}.wav'
    if not media.exists():
        os.link(audio, media)
    tracks.append(dict(id=index + 1, uri=media.resolve().as_uri(), source='music',
                       cat='Soundtrack', art=f'Artist {index % 200:03}', alb=f'Album {index % 400:03}',
                       name=f'Track {index:05} — long playback title for native marquee measurement',
                       duration=dict(secs=3600, nanos=0), track_no=index % 12 + 1, disc_no=1,
                       play_count=0, last_played_at=None, added_at=None, release_date=None,
                       kind='WAV', bitrate_kbps=128, rating=None, orig_cat=None))
settings = dict(theme='light', zoom=1, zebra=True, columnOrder=['track', 'name', 'artist', 'album', 'time', 'plays', 'rating', 'genre'],
                autoAddSpotifyLibrary=False, autoConnect=False, spotifyClientId='', volume=0, lastfmScrobbling=False)
uris = [track['uri'] for track in tracks] + [track['uri'] for track in tracks[:100]]
playlists = dict(playlists=[dict(id='fixture', name='Performance playlist', snapshot_id='fixture', owned=True,
                               owner=None, track_count=len(uris), tracks=uris, track_metadata_version=1, spotify_tracks=[])])
for name, value in [('library.json', dict(version=1, library=dict(tracks=tracks, album_ratings=[], next_id=args.count + 1))), ('settings.json', settings), ('playlists.json', playlists)]:
    temporary = args.data_dir / f'{name}.tmp'
    temporary.write_text(json.dumps(value))
    os.replace(temporary, args.data_dir / name)
print(json.dumps(dict(tracks=args.count, audioSeconds=3600, dataDir=str(args.data_dir), mediaDir=str(args.media_dir))))
