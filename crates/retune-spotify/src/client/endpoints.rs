use super::*;

impl<T: Transport, S: TokenStore> SpotifyClient<T, S> {
    pub async fn saved_tracks(&self, offset: u32, limit: u32) -> Result<Page<SavedTrack>> {
        let page: Page<SavedTrack> = self.get(&paged("/me/tracks", offset, limit)).await?;
        let mut catalog = self.catalog.lock().expect("Spotify catalog mutex poisoned");
        for saved in &page.items {
            catalog.observe_track(&saved.track);
        }
        Ok(page)
    }

    pub async fn me(&self) -> Result<Profile> {
        self.get("/me").await
    }

    pub async fn playlists(
        &self,
        offset: u32,
        limit: u32,
        current_user_id: &str,
    ) -> Result<Page<Playlist>> {
        let mut page: Page<Playlist> = self.get(&paged("/me/playlists", offset, limit)).await?;
        for playlist in &mut page.items {
            playlist.owned = playlist.owner.id == current_user_id;
        }
        Ok(page)
    }

    pub async fn create_playlist(&self, name: &str) -> Result<CreatedPlaylist> {
        self.json(
            Method::Post,
            "/me/playlists",
            serde_json::to_vec(&CreatePlaylist {
                name,
                public: false,
            })
            .expect("playlist create body serializes"),
        )
        .await
    }

    pub async fn unfollow_playlist(&self, playlist_id: &str) -> Result<()> {
        let playlist_id = spotify_path_id(playlist_id, "playlist")?;
        self.empty(
            Method::Delete,
            &format!("/playlists/{playlist_id}/followers"),
            Vec::new(),
        )
        .await
    }

    pub async fn playlist_tracks(
        &self,
        playlist_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<Track>> {
        let playlist_id = spotify_path_id(playlist_id, "playlist")?;
        let fields = "items(is_local,item(uri,name,artists(id,name),album(id,uri,name,images(url)),duration_ms)),next,total";
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("offset", &offset.to_string())
            .append_pair("limit", &limit.to_string())
            .append_pair("fields", fields)
            .finish();
        let page: Page<PlaylistTrackItem> = self
            .get(&format!("/playlists/{playlist_id}/items?{query}"))
            .await?;
        let total = page.total;
        let next = page.next;
        let mut skipped = page.skipped;
        let items = page
            .items
            .into_iter()
            .filter_map(|item| {
                if item.is_local || item.track.is_none() {
                    skipped += 1;
                    None
                } else {
                    item.track
                }
            })
            .collect();
        let page = Page {
            items,
            next,
            skipped,
            total,
        };
        let mut catalog = self.catalog.lock().expect("Spotify catalog mutex poisoned");
        for track in &page.items {
            catalog.observe_track_summary(track);
        }
        Ok(page)
    }

    pub async fn add_playlist_tracks(
        &self,
        playlist_id: &str,
        uris: &[String],
        position: Option<u32>,
    ) -> Result<String> {
        let playlist_id = spotify_path_id(playlist_id, "playlist")?;
        reject_local_uris(uris)?;
        if uris.is_empty() {
            return Err(Error::InvalidRequest(
                "playlist add requires at least one URI".into(),
            ));
        }
        let mut snapshot_id = String::new();
        for (chunk_index, chunk) in uris.chunks(100).enumerate() {
            let position =
                position.map(|position| position.saturating_add((chunk_index * 100) as u32));
            let response: SnapshotResponse = self
                .json(
                    Method::Post,
                    &format!("/playlists/{playlist_id}/items"),
                    serde_json::to_vec(&AddPlaylistTracks {
                        uris: chunk,
                        position,
                    })
                    .expect("playlist add body serializes"),
                )
                .await?;
            snapshot_id = response.snapshot_id;
        }
        Ok(snapshot_id)
    }

    pub async fn reorder_playlist_tracks(
        &self,
        playlist_id: &str,
        range_start: u32,
        insert_before: u32,
        range_length: u32,
        snapshot_id: &str,
    ) -> Result<String> {
        let playlist_id = spotify_path_id(playlist_id, "playlist")?;
        self.json::<SnapshotResponse>(
            Method::Put,
            &format!("/playlists/{playlist_id}/items"),
            serde_json::to_vec(&ReorderPlaylistTracks {
                range_start,
                insert_before,
                range_length,
                snapshot_id,
            })
            .expect("playlist reorder body serializes"),
        )
        .await
        .map(|response| response.snapshot_id)
    }

    pub async fn remove_playlist_tracks(
        &self,
        playlist_id: &str,
        uris: &[String],
        snapshot_id: &str,
    ) -> Result<String> {
        let playlist_id = spotify_path_id(playlist_id, "playlist")?;
        reject_local_uris(uris)?;
        if uris.is_empty() {
            return Err(Error::InvalidRequest(
                "playlist remove requires at least one URI".into(),
            ));
        }
        let mut snapshot_id = snapshot_id.to_owned();
        for chunk in uris.chunks(100) {
            snapshot_id = self
                .json::<SnapshotResponse>(
                    Method::Delete,
                    &format!("/playlists/{playlist_id}/items"),
                    serde_json::to_vec(&RemovePlaylistTracks {
                        items: chunk.iter().map(|uri| PlaylistTrackUri { uri }).collect(),
                        snapshot_id: &snapshot_id,
                    })
                    .expect("playlist remove body serializes"),
                )
                .await?
                .snapshot_id;
        }
        Ok(snapshot_id)
    }

    pub async fn saved_albums(&self, offset: u32, limit: u32) -> Result<Page<SavedAlbum>> {
        let page: Page<SavedAlbum> = self.get(&paged("/me/albums", offset, limit)).await?;
        let mut catalog = self.catalog.lock().expect("Spotify catalog mutex poisoned");
        for saved in &page.items {
            catalog.observe_album_summary(&saved.album);
        }
        Ok(page)
    }

    pub async fn saved_shows(&self, offset: u32, limit: u32) -> Result<Page<SavedShow>> {
        self.get(&paged("/me/shows", offset, limit)).await
    }

    pub async fn show_episodes(
        &self,
        show_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<Episode>> {
        let show_id = spotify_path_id(show_id, "show")?;
        self.get(&paged(&format!("/shows/{show_id}/episodes"), offset, limit))
            .await
    }

    pub async fn saved_episodes(&self, offset: u32, limit: u32) -> Result<Page<SavedEpisode>> {
        self.get(&paged("/me/episodes", offset, limit)).await
    }

    pub async fn saved_audiobooks(&self, offset: u32, limit: u32) -> Result<Page<Audiobook>> {
        self.get(&paged("/me/audiobooks", offset, limit)).await
    }

    pub async fn audiobook_chapters(
        &self,
        audiobook_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<Chapter>> {
        let audiobook_id = spotify_path_id(audiobook_id, "audiobook")?;
        self.get(&paged(
            &format!("/audiobooks/{audiobook_id}/chapters"),
            offset,
            limit,
        ))
        .await
    }

    pub async fn artist(&self, id: &str) -> Result<Artist> {
        let id = spotify_path_id(id, "artist")?;
        if let Some(artist) = self
            .catalog
            .lock()
            .expect("Spotify catalog mutex poisoned")
            .complete_artist(id)
        {
            log::info!("Spotify catalog cache hit kind=artist");
            return Ok(artist);
        }
        let artist: Artist = self.get(&format!("/artists/{id}")).await?;
        self.catalog
            .lock()
            .expect("Spotify catalog mutex poisoned")
            .observe_artist(&artist);
        Ok(artist)
    }

    pub async fn album(&self, id: &str) -> Result<Album> {
        let id = spotify_path_id(id, "album")?;
        let uri = format!("spotify:album:{id}");
        if let Some(album) = self
            .catalog
            .lock()
            .expect("Spotify catalog mutex poisoned")
            .complete_album(&uri)
        {
            log::info!("Spotify catalog cache hit kind=album");
            return Ok(album);
        }
        let mut album: Album = self.get(&format!("/albums/{id}")).await?;
        let Some(mut tracks) = album.tracks.take() else {
            self.catalog
                .lock()
                .expect("Spotify catalog mutex poisoned")
                .observe_album(&album, false);
            return Ok(album);
        };
        let mut offset = (tracks.items.len() + tracks.skipped) as u32;
        while tracks.next.is_some() && offset < tracks.total {
            let page = self.album_tracks(id, offset, 50).await?;
            let count = (page.items.len() + page.skipped) as u32;
            tracks.items.extend(page.items);
            tracks.skipped += page.skipped;
            tracks.next = page.next;
            tracks.total = page.total;
            if count == 0 {
                break;
            }
            offset += count;
        }
        let complete = tracks.next.is_none()
            && tracks.skipped == 0
            && tracks.items.len() as u32 == tracks.total;
        if complete {
            let parent = AlbumSummary {
                id: album.id.clone(),
                uri: album.uri.clone(),
                name: album.name.clone(),
                release_date: album.release_date.clone(),
                images: album.images.clone(),
            };
            for track in &mut tracks.items {
                track.album = Some(parent.clone());
            }
        }
        album.tracks = Some(tracks);
        self.catalog
            .lock()
            .expect("Spotify catalog mutex poisoned")
            .observe_album(&album, complete);
        Ok(album)
    }

    pub async fn track(&self, id: &str) -> Result<Track> {
        let id = spotify_path_id(id, "track")?;
        let uri = format!("spotify:track:{id}");
        if let Some(track) = self
            .catalog
            .lock()
            .expect("Spotify catalog mutex poisoned")
            .complete_track(&uri)
        {
            log::info!("Spotify catalog cache hit kind=track");
            return Ok(track);
        }
        let track: Track = self.get(&format!("/tracks/{id}")).await?;
        self.catalog
            .lock()
            .expect("Spotify catalog mutex poisoned")
            .observe_track(&track);
        Ok(track)
    }

    pub async fn is_following_artist(&self, id: &str) -> Result<bool> {
        let id = spotify_path_id(id, "artist")?;
        let uri = format!("spotify:artist:{id}");
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("uris", &uri)
            .finish();
        self.get::<[bool; 1]>(&format!("/me/library/contains?{query}"))
            .await
            .map(|[following]| following)
    }

    pub async fn follow_artist(&self, id: &str, follow: bool) -> Result<()> {
        let id = spotify_path_id(id, "artist")?;
        self.change_library(
            if follow { Method::Put } else { Method::Delete },
            &[format!("spotify:artist:{id}")],
        )
        .await
    }

    pub async fn search(&self, query: &str, offset: u32, limit: u32) -> Result<SearchResults> {
        self.search_with_types(query, "artist,album,track", offset, limit)
            .await
    }

    pub async fn search_with_types(
        &self,
        query: &str,
        types: &str,
        offset: u32,
        limit: u32,
    ) -> Result<SearchResults> {
        validate_search_input(query, offset)?;
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("q", query)
            .append_pair("type", types)
            .append_pair("offset", &offset.to_string())
            .append_pair("limit", &limit.min(10).to_string())
            .finish();
        let results: SearchResults = self.get(&format!("/search?{query}")).await?;
        let mut catalog = self.catalog.lock().expect("Spotify catalog mutex poisoned");
        for artist in &results.artists.items {
            catalog.observe_artist_summary(artist);
        }
        for album in &results.albums.items {
            catalog.observe_album_summary(album);
        }
        for track in &results.tracks.items {
            catalog.observe_track(track);
        }
        Ok(results)
    }

    pub async fn artist_albums(
        &self,
        artist_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<Album>> {
        let artist_id = spotify_path_id(artist_id, "artist")?;
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("include_groups", "album,single")
            .append_pair("offset", &offset.to_string())
            .append_pair("limit", &limit.to_string())
            .finish();
        let page: Page<Album> = self
            .get(&format!("/artists/{artist_id}/albums?{query}"))
            .await?;
        let mut catalog = self.catalog.lock().expect("Spotify catalog mutex poisoned");
        for album in &page.items {
            catalog.observe_album_summary(album);
        }
        Ok(page)
    }

    pub async fn album_tracks(
        &self,
        album_id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<Page<Track>> {
        let album_id = spotify_path_id(album_id, "album")?;
        let uri = format!("spotify:album:{album_id}");
        if let Some(page) = self
            .catalog
            .lock()
            .expect("Spotify catalog mutex poisoned")
            .complete_album_tracks(&uri, offset, limit)
        {
            log::info!("Spotify catalog cache hit kind=album_tracks");
            return Ok(page);
        }
        let page: Page<Track> = self
            .get(&paged(&format!("/albums/{album_id}/tracks"), offset, limit))
            .await?;
        let complete = offset == 0
            && page.next.is_none()
            && page.skipped == 0
            && page.items.len() as u32 == page.total;
        self.catalog
            .lock()
            .expect("Spotify catalog mutex poisoned")
            .observe_album_track_page(&uri, offset, &page, complete);
        Ok(page)
    }

    pub async fn player(&self) -> Result<Option<PlayerState>> {
        let response = self
            .api_request(Method::Get, "/me/player", Vec::new())
            .await?;
        if response.status == 204 || response.body.is_empty() {
            return Ok(None);
        }
        decode("/me/player", &response.body).map(Some)
    }

    pub async fn devices(&self) -> Result<Vec<Device>> {
        self.get::<Devices>("/me/player/devices")
            .await
            .map(|response| response.devices)
    }

    pub async fn play(
        &self,
        device_id: Option<&str>,
        uris: &[String],
        offset: usize,
    ) -> Result<()> {
        reject_local_uris(uris)?;
        if offset >= uris.len() {
            return Err(Error::InvalidRequest(
                "play offset is outside the queue".into(),
            ));
        }
        let path = player_path("/me/player/play", &[], device_id);
        self.empty(
            Method::Put,
            &path,
            serde_json::to_vec(&serde_json::json!({
                "uris": uris,
                "offset": { "position": offset }
            }))
            .expect("play body serializes"),
        )
        .await
    }

    pub async fn set_repeat(&self, state: &str, device_id: Option<&str>) -> Result<()> {
        if !matches!(state, "off" | "context" | "track") {
            return Err(Error::InvalidRequest("invalid repeat state".into()));
        }
        let path = player_path("/me/player/repeat", &[("state", state)], device_id);
        self.empty(Method::Put, &path, Vec::new()).await
    }

    pub async fn resume(&self, device_id: Option<&str>) -> Result<()> {
        self.empty(
            Method::Put,
            &player_path("/me/player/play", &[], device_id),
            Vec::new(),
        )
        .await
    }

    pub async fn pause(&self, device_id: Option<&str>) -> Result<()> {
        self.empty(
            Method::Put,
            &player_path("/me/player/pause", &[], device_id),
            Vec::new(),
        )
        .await
    }

    pub async fn set_volume(&self, volume_percent: u8, device_id: Option<&str>) -> Result<()> {
        if volume_percent > 100 {
            return Err(Error::InvalidRequest(
                "volume percent must be between 0 and 100".into(),
            ));
        }
        let path = player_path(
            "/me/player/volume",
            &[("volume_percent", &volume_percent.to_string())],
            device_id,
        );
        self.empty(Method::Put, &path, Vec::new()).await
    }

    pub async fn seek(&self, position_ms: u32, device_id: Option<&str>) -> Result<()> {
        let path = player_path(
            "/me/player/seek",
            &[("position_ms", &position_ms.to_string())],
            device_id,
        );
        self.empty(Method::Put, &path, Vec::new()).await
    }

    pub async fn transfer(&self, device_id: &str, play: bool) -> Result<()> {
        self.empty(
            Method::Put,
            "/me/player",
            serde_json::to_vec(&serde_json::json!({
                "device_ids": [device_id],
                "play": play
            }))
            .expect("transfer body serializes"),
        )
        .await
    }

    pub async fn save_to_library(&self, uris: &[String]) -> Result<()> {
        self.change_library(Method::Put, uris).await
    }

    pub async fn remove_from_library(&self, uris: &[String]) -> Result<()> {
        self.change_library(Method::Delete, uris).await
    }

    async fn change_library(&self, method: Method, uris: &[String]) -> Result<()> {
        if uris.len() > MAX_LIBRARY_WRITE_URIS {
            return Err(Error::InvalidRequest(format!(
                "library write exceeds {MAX_LIBRARY_WRITE_URIS} URIs"
            )));
        }
        reject_local_uris(uris)?;
        for chunk in uris.chunks(40) {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("uris", &chunk.join(","))
                .finish();
            self.empty(method, &format!("/me/library?{query}"), Vec::new())
                .await?;
        }
        Ok(())
    }
}
