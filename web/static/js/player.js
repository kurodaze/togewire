(function () {
    const $ = id => document.getElementById(id);
    const el = { status: $('status'), name: $('trackName'), artist: $('trackArtist'), progressRow: $('progressRow'), progress: $('progress'), btn: $('btn'), volumeControl: $('volumeControl'), volumeToggle: $('volumeToggle'), volume: $('volume') };

    let ws, audioEl = null, listening = false, track = null;
    let reconnectTimer = null, reconnectDelay = 1000, progressTimer = null;
    let connectedStreamGen = 0, pauseTimeout = null;
    let lastVolume = Number(el.volume.value) || 70;

    function fmt(ms) {
        const s = Math.floor(ms / 1000);
        return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
    }

    function show(e) { e.classList.remove('hidden'); }
    function hide(e) { e.classList.add('hidden'); }

    function syncVolume() {
        const level = Number(el.volume.value);
        const muted = level === 0;
        if (!muted) lastVolume = level;
        el.volumeToggle.classList.toggle('muted', muted);
        el.volumeToggle.setAttribute('aria-label', muted ? 'unmute volume' : 'mute volume');
        if (audioEl) audioEl.volume = level / 100;
    }

    function toggleMute() {
        if (Number(el.volume.value) === 0) {
            el.volume.value = String(lastVolume);
        } else {
            lastVolume = Number(el.volume.value);
            el.volume.value = '0';
        }
        syncVolume();
    }

    // UI: reflects server state

    function render() {
        if (!track || !track.track) {
            el.status.textContent = 'not playing';
            hide(el.name); hide(el.artist); hide(el.progressRow); hide(el.btn); hide(el.volumeControl);
            if (listening) stopListening();
            return;
        }

        const t = track.track;
        let statusText = track.isPlaying ? 'now playing' : 'paused';
        el.status.classList.remove('playing');

        if (listening && track.isPlaying && !track.stream_active) {
            if (track.preparing) statusText = 'preparing track...';
            else if (track.no_source) statusText = 'audio not found';
            else statusText = 'waiting for audio...';
        } else {
            const c = track.listener_count || 0;
            if (listening && c > 1) {
                statusText = `listening among ${c - 1} other${c > 2 ? 's' : ''}`;
            } else if (track.isPlaying) {
                el.status.classList.add('playing');
            }
        }

        el.status.textContent = statusText;

        el.name.textContent = t.name;
        el.artist.textContent = t.artist;
        show(el.name); show(el.artist);

        renderProgress();
        show(el.progressRow);

        if (track.isPlaying && !progressTimer) progressTimer = setInterval(renderProgress, 1000);
        if (!track.isPlaying && progressTimer) { clearInterval(progressTimer); progressTimer = null; }

        // Always show button if there's a track (listening persists through pause)
        show(el.btn);
        if (listening) show(el.volumeControl);
        else hide(el.volumeControl);
    }

    function renderProgress() {
        if (!track || !track.track) return;
        let p = track.progress || 0;
        if (track.isPlaying && track._ts) p += Date.now() - track._ts;
        const d = track.track.duration || 0;
        if (d > 0) el.progress.textContent = `${fmt(Math.min(p, d))} / ${fmt(d)}`;
    }

    // Audio: single persistent element, created on user gesture

    function connectStream() {
        if (!audioEl) return;
        syncVolume();
        audioEl.src = `/stream?t=${Date.now()}`;
        audioEl.play().catch(() => { });
        connectedStreamGen = track?.stream_gen || 0;
    }

    function disconnectStream() {
        if (!audioEl) return;
        audioEl.pause();
        audioEl.removeAttribute('src');
        audioEl.load();
        connectedStreamGen = 0;
    }

    // Listener toggle

    function sendListening(on) {
        if (ws && ws.readyState === WebSocket.OPEN) {
            ws.send(JSON.stringify({ type: 'listening', listening: on }));
        }
    }

    function startListening() {
        listening = true;
        sendListening(true);
        el.btn.textContent = 'leave';
        el.btn.classList.add('active');
        // Create Audio element on user gesture (click) to satisfy autoplay policy
        if (!audioEl) {
            audioEl = new Audio();
        }
        syncVolume();
        if (track?.stream_active && track?.isPlaying) connectStream();
        render();
    }

    function stopListening() {
        listening = false;
        sendListening(false);
        el.btn.textContent = 'listen along';
        el.btn.classList.remove('active');
        if (pauseTimeout) { clearTimeout(pauseTimeout); pauseTimeout = null; }
        disconnectStream();
        render();
    }

    el.btn.addEventListener('click', () => listening ? stopListening() : startListening());
    el.volumeToggle.addEventListener('click', toggleMute);
    el.volume.addEventListener('input', syncVolume);

    // React to every server push

    function syncAudio() {
        if (!listening || !audioEl) return;

        const shouldPlay = track?.stream_active && track?.isPlaying;
        const serverGen = track?.stream_gen || 0;

        if (!shouldPlay) {
            // Pause audio but keep listening state, auto-leave after 60s
            if (!audioEl.paused) audioEl.pause();
            if (!pauseTimeout) {
                pauseTimeout = setTimeout(() => {
                    pauseTimeout = null;
                    if (listening && !track?.isPlaying) stopListening();
                }, 60000);
            }
            return;
        }

        // Resuming or playing, clear pause timeout
        if (pauseTimeout) { clearTimeout(pauseTimeout); pauseTimeout = null; }

        // Connect or reconnect if generation changed
        if (!connectedStreamGen || serverGen !== connectedStreamGen) {
            connectStream();
        }
    }

    // WebSocket

    function onMessage(data) {
        if (data.type === 'track_update') {
            track = { ...data.data, _ts: Date.now() };
            syncAudio();
            render();
        }
    }

    function connect() {
        try {
            const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
            ws = new WebSocket(`${proto}//${location.host}/ws`);
            ws.onopen = () => {
                if (!track || !track.track) el.status.textContent = 'not playing';
                reconnectDelay = 1000;
                if (listening) sendListening(true);
            };
            ws.onmessage = e => onMessage(JSON.parse(e.data));
            ws.onclose = () => { el.status.textContent = 'disconnected'; scheduleReconnect(); };
        } catch (_) { scheduleReconnect(); }
    }

    function scheduleReconnect() {
        if (reconnectTimer) return;
        reconnectTimer = setTimeout(() => {
            reconnectTimer = null;
            reconnectDelay = Math.min(reconnectDelay * 2, 30000);
            connect();
        }, reconnectDelay);
    }

    syncVolume();
    connect();
})();
