(function () {
  // Codes, not text, so a crafted link cannot show its own message here.
  const NOTICES = new Map([
    ["paired", "Paired with Spotify"],
    ["disconnected", "Disconnected from Spotify"],
    ["bad_bitrate", "Invalid audio quality"],
    ["togematch_ok", "Connected"],
    ["togematch_failed", "Connection failed"],
    ["save_failed", "Couldn't save the settings"],
  ]);
  const notice = NOTICES.get(new URLSearchParams(window.location.search).get("n"));
  if (notice) alert(notice);
  if (window.location.search) {
    window.history.replaceState({}, document.title, window.location.pathname + window.location.hash);
  }

  for (const sw of document.querySelectorAll(".field > .switch")) {
    const input = sw.querySelector('input[type="checkbox"]');
    const state = sw.querySelector(".switch-state");
    if (!input || !state) continue;
    const sync = () => {
      state.textContent = input.checked ? "On" : "Off";
    };
    input.addEventListener("change", sync);
    sync();
  }

  const flashSectionNote = (el, text, state = "", ms = 1500) => {
    const heading = el && el.closest(".section")?.querySelector("h2");
    if (!heading) return;
    if (heading._noteTimer) {
      clearTimeout(heading._noteTimer);
      heading._noteTimer = null;
    }
    heading.querySelector(".saved")?.remove();
    const note = document.createElement("span");
    note.className = state ? `saved is-${state}` : "saved";
    note.textContent = text;
    heading.append(note);
    if (ms > 0) {
      heading._noteTimer = setTimeout(() => {
        note.remove();
        heading._noteTimer = null;
      }, ms);
    }
  };

  const redirectIfLoggedOut = (res) => {
    if (res && res.redirected && res.url && res.url.includes("/admin/login")) {
      window.location.href = res.url;
      return true;
    }
    return false;
  };

  // Settings save as they change, like the player switch: no Save button to miss.
  for (const form of document.querySelectorAll("#settings-form, #togematch-form")) {
    const isTogematch = form.id === "togematch-form";
    let lastSubmitted = new URLSearchParams(new FormData(form)).toString();
    const save = async (field, force = false) => {
      const target = field || form.querySelector("input, select");
      if (!form.reportValidity()) return;
      const body = new URLSearchParams(new FormData(form));
      const serialized = body.toString();
      if (!force && serialized === lastSubmitted) return;
      lastSubmitted = serialized;
      let testingTimer = null;
      if (isTogematch && form.querySelector("#togematch_api")?.value.trim()) {
        // Delay "Testing..." so fast LAN responses go straight to the result without flickering.
        testingTimer = setTimeout(() => {
          testingTimer = null;
          flashSectionNote(target, "Testing...", "", 0);
        }, 250);
      }
      let code;
      try {
        const res = await fetch(form.action, { method: "POST", body });
        if (redirectIfLoggedOut(res)) return;
        code = new URL(res.url).searchParams.get("n");
      } catch {
        code = isTogematch ? "togematch_failed" : "save_failed";
      } finally {
        if (testingTimer) clearTimeout(testingTimer);
      }
      if (code === "saved") {
        flashSectionNote(target, "Saved");
      } else if (code === "togematch_ok") {
        flashSectionNote(target, "Connected", "ok", 2500);
      } else if (code === "togematch_failed") {
        flashSectionNote(target, "Connection failed", "err", 3000);
      } else {
        alert(NOTICES.get(code) || NOTICES.get("save_failed"));
      }
    };
    form.addEventListener("change", (e) => save(e.target));
    form.addEventListener("submit", (e) => {
      e.preventDefault();
      save(document.activeElement, true);
    });
  }

  const viewCacheBtn = document.getElementById("view-cache-btn");
  const panelCache = document.getElementById("panel-cache");
  let openCachePanel = null;
  let leaveCachePanel = () => {};
  if (panelCache) {
    const searchInput = document.getElementById("cache-search");
    const qualityFilter = document.getElementById("cache-filter-quality");
    const sourceFilter = document.getElementById("cache-filter-source");
    const listEl = document.getElementById("cache-list");
    const emptyEl = document.getElementById("cache-empty");
    const summaryEl = document.getElementById("cache-panel-summary");
    const usageTextEl = document.getElementById("storage-usage-text");
    const previewEl = document.getElementById("cache-cover-preview");
    const previewImg = document.getElementById("cache-cover-preview-img");
    const selectionBar = document.getElementById("cache-selection-bar");
    const selectAllBtn = document.getElementById("cache-select-all");
    const selectionCountEl = document.getElementById("cache-selection-count");
    const bulkDownloadBtn = document.getElementById("cache-bulk-download");
    const bulkDeleteBtn = document.getElementById("cache-bulk-delete");
    const bulkDownloadForm = document.getElementById("cache-bulk-download-form");
    const bulkDownloadIdsInput = document.getElementById("cache-bulk-download-ids");

    let cachedItems = [];
    let visibleItems = [];
    let cacheUsedMb = "0.0";
    let cacheEntries = 0;
    const selectedIds = new Set();
    let anchorId = null;
    let anchorState = true;

    const formatTrackCount = (n) => `${n} ${n === 1 ? "track" : "tracks"}`;
    const formatSize = (bytes) => {
      const n = Number(bytes) || 0;
      if (n >= 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
      if (n >= 1024) return `${Math.max(1, Math.round(n / 1024))} KB`;
      return `${n} B`;
    };
    const normalizeSourceKey = (src) => {
      const lower = (src || "").toLowerCase();
      if (lower === "librespot" || lower === "spotify") return "spotify";
      if (lower === "ytdlp" || lower === "youtube") return "youtube";
      return "";
    };
    const formatSource = (src) => {
      const key = normalizeSourceKey(src);
      if (key === "spotify") return "Spotify";
      if (key === "youtube") return "YouTube";
      return src || "";
    };

    const syncFilterOptions = () => {
      if (qualityFilter) {
        const current = qualityFilter.value;
        const rates = new Set();
        for (const item of cachedItems) {
          if (item.bitrate) rates.add(item.bitrate);
        }
        const sorted = [...rates].sort((a, b) => (parseInt(a, 10) || 0) - (parseInt(b, 10) || 0));
        qualityFilter.replaceChildren();
        const allOpt = document.createElement("option");
        allOpt.value = "";
        allOpt.textContent = "All qualities";
        qualityFilter.append(allOpt);
        for (const rate of sorted) {
          const opt = document.createElement("option");
          opt.value = rate;
          opt.textContent = `${rate} Opus`;
          qualityFilter.append(opt);
        }
        qualityFilter.value = rates.has(current) ? current : "";
      }
      if (sourceFilter) {
        const current = sourceFilter.value;
        const present = new Set();
        for (const item of cachedItems) {
          present.add(normalizeSourceKey(item.source) || "unknown");
        }
        sourceFilter.replaceChildren();
        const opts = [["", "All sources"]];
        if (present.has("spotify")) opts.push(["spotify", "Spotify"]);
        if (present.has("youtube")) opts.push(["youtube", "YouTube"]);
        if (present.has("unknown")) opts.push(["unknown", "Unknown"]);
        for (const [val, label] of opts) {
          const opt = document.createElement("option");
          opt.value = val;
          opt.textContent = label;
          sourceFilter.append(opt);
        }
        sourceFilter.value = opts.some(([v]) => v === current) ? current : "";
      }
    };

    const syncUsageText = () => {
      if (usageTextEl) {
        usageTextEl.textContent = `${cacheUsedMb} MB used (${formatTrackCount(cacheEntries)})`;
      }
    };

    const hidePreview = () => {
      if (!previewEl) return;
      previewEl.classList.remove("is-visible");
      previewEl.hidden = true;
    };

    const syncSelectionUi = () => {
      const count = selectedIds.size;
      const hasSelection = count > 0;
      if (hasSelection && searchInput && document.activeElement === searchInput) {
        searchInput.blur();
      }
      panelCache.classList.toggle("has-selection", hasSelection);
      if (selectionBar) {
        selectionBar.hidden = !hasSelection;
      }
      if (listEl) {
        for (const row of listEl.children) {
          const id = row.dataset.trackId;
          const isSel = selectedIds.has(id);
          row.classList.toggle("is-selected", isSel);
          const cb = row.querySelector(".cache-row-check");
          if (cb) cb.checked = isSel;
        }
      }
      let totalBytes = 0;
      if (hasSelection) {
        for (const item of cachedItems) {
          if (selectedIds.has(item.id)) totalBytes += Number(item.file_size) || 0;
        }
      }
      if (selectionCountEl) {
        selectionCountEl.textContent = hasSelection
          ? `${count} selected (${formatSize(totalBytes)})`
          : "0 selected";
      }
      if (bulkDownloadBtn) bulkDownloadBtn.disabled = !hasSelection;
      if (bulkDeleteBtn) bulkDeleteBtn.disabled = !hasSelection;
      if (selectAllBtn) {
        const allVisibleSelected =
          visibleItems.length > 0 && visibleItems.every((item) => selectedIds.has(item.id));
        selectAllBtn.textContent = allVisibleSelected ? "Deselect all" : "Select all";
        selectAllBtn.disabled = visibleItems.length === 0;
      }
    };

    const clearSelection = () => {
      selectedIds.clear();
      anchorId = null;
      anchorState = true;
      syncSelectionUi();
    };

    leaveCachePanel = () => {
      hidePreview();
      if (selectedIds.size > 0) clearSelection();
    };

    const handleRowSelect = (id, shiftKey) => {
      if (shiftKey && anchorId) {
        const anchorIdx = visibleItems.findIndex((x) => x.id === anchorId);
        const clickedIdx = visibleItems.findIndex((x) => x.id === id);
        if (anchorIdx !== -1 && clickedIdx !== -1) {
          const start = Math.min(anchorIdx, clickedIdx);
          const end = Math.max(anchorIdx, clickedIdx);
          for (let i = start; i <= end; i += 1) {
            if (anchorState) selectedIds.add(visibleItems[i].id);
            else selectedIds.delete(visibleItems[i].id);
          }
          syncSelectionUi();
          return;
        }
      }
      const next = !selectedIds.has(id);
      if (next) selectedIds.add(id);
      else selectedIds.delete(id);
      anchorId = id;
      anchorState = next;
      syncSelectionUi();
    };

    const showPreview = (anchor, src) => {
      if (!previewEl || !previewImg || !src) return;
      if (previewImg.src !== src) previewImg.src = src;
      const size = 220;
      const pad = 12;
      const rect = anchor.getBoundingClientRect();
      let left = rect.right + 10;
      if (left + size + pad > window.innerWidth) {
        left = rect.left - size - 10;
      }
      left = Math.max(pad, Math.min(left, window.innerWidth - size - pad));
      let top = rect.top + rect.height / 2 - size / 2;
      top = Math.max(pad, Math.min(top, window.innerHeight - size - pad));
      previewEl.style.left = `${Math.round(left)}px`;
      previewEl.style.top = `${Math.round(top)}px`;
      previewEl.hidden = false;
      requestAnimationFrame(() => {
        if (!previewEl.hidden) previewEl.classList.add("is-visible");
      });
    };

    const renderCacheList = () => {
      hidePreview();
      const q = (searchInput ? searchInput.value : "").trim().toLowerCase();
      const quality = qualityFilter ? qualityFilter.value : "";
      const source = sourceFilter ? sourceFilter.value : "";
      const isFiltered = !!(q || quality || source);
      const filtered = isFiltered
        ? cachedItems.filter((item) => {
            if (q && !(item.name || "").toLowerCase().includes(q)) return false;
            if (quality && item.bitrate !== quality) return false;
            if (source) {
              const key = normalizeSourceKey(item.source) || "unknown";
              if (key !== source) return false;
            }
            return true;
          })
        : cachedItems;
      visibleItems = filtered;

      if (summaryEl) {
        summaryEl.textContent = isFiltered
          ? `${filtered.length} of ${formatTrackCount(cacheEntries)} · ${cacheUsedMb} MB`
          : `${formatTrackCount(cacheEntries)} · ${cacheUsedMb} MB`;
      }

      if (!listEl || !emptyEl) return;
      listEl.replaceChildren();
      if (filtered.length === 0) {
        listEl.hidden = true;
        emptyEl.textContent =
          cachedItems.length === 0 ? "No cached tracks." : "No matching tracks.";
        emptyEl.hidden = false;
        syncSelectionUi();
        return;
      }
      listEl.hidden = false;
      emptyEl.hidden = true;

      const frag = document.createDocumentFragment();
      for (const item of filtered) {
        const row = document.createElement("div");
        row.className = "cache-row";
        row.dataset.trackId = item.id;
        row.setAttribute("role", "listitem");

        const checkbox = document.createElement("input");
        checkbox.type = "checkbox";
        checkbox.className = "cache-row-check";
        checkbox.checked = selectedIds.has(item.id);
        checkbox.setAttribute("aria-label", `Select ${item.name}`);

        row.addEventListener("click", (e) => {
          if (e.target.closest(".cache-cover-link") || e.target.closest(".cache-row-actions")) {
            return;
          }
          handleRowSelect(item.id, e.shiftKey);
        });

        const coverUrl = `/admin/cache/${encodeURIComponent(item.id)}/cover`;
        const thumbUrl = `${coverUrl}?s=${encodeURIComponent(String(item.file_size || 0))}`;
        const coverLink = document.createElement("a");
        coverLink.className = "cache-cover-link";
        coverLink.href = coverUrl;
        coverLink.target = "_blank";
        coverLink.rel = "noopener";
        coverLink.title = "Open full cover art";
        coverLink.setAttribute("aria-label", `Open cover art for ${item.name}`);

        const img = document.createElement("img");
        img.className = "cache-cover-img";
        img.src = thumbUrl;
        img.alt = "";
        img.loading = "lazy";
        img.decoding = "async";
        img.addEventListener("load", () => {
          coverLink.classList.add("is-loaded");
        });
        img.addEventListener("error", () => {
          coverLink.classList.add("is-missing");
          coverLink.removeAttribute("href");
          coverLink.removeAttribute("title");
        });

        const fallback = document.createElement("span");
        fallback.className = "cache-cover-fallback";
        fallback.textContent = "♪";
        fallback.setAttribute("aria-hidden", "true");
        coverLink.append(img, fallback);

        const openPreview = () => {
          if (coverLink.classList.contains("is-loaded")) {
            showPreview(coverLink, img.currentSrc || thumbUrl);
          }
        };
        coverLink.addEventListener("mouseenter", openPreview);
        coverLink.addEventListener("focusin", openPreview);
        coverLink.addEventListener("mouseleave", hidePreview);
        coverLink.addEventListener("blur", hidePreview);

        const info = document.createElement("div");
        info.className = "cache-row-info";
        const nameEl = document.createElement("p");
        nameEl.className = "cache-row-name";
        nameEl.textContent = item.name;
        nameEl.title = item.name;

        const metaParts = [formatSize(item.file_size)];
        if (item.bitrate) metaParts.push(`${item.bitrate} Opus`);
        const srcLabel = formatSource(item.source);
        if (srcLabel) metaParts.push(srcLabel);
        if (item.normalized) metaParts.push("Normalized");

        const metaEl = document.createElement("p");
        metaEl.className = "cache-row-meta";
        metaEl.textContent = metaParts.join(" · ");
        info.append(nameEl, metaEl);

        const actions = document.createElement("div");
        actions.className = "cache-row-actions";

        const playLink = document.createElement("a");
        playLink.className = "btn btn-sm";
        playLink.href = `/admin/cache/${encodeURIComponent(item.id)}/file?inline=1`;
        playLink.target = "_blank";
        playLink.rel = "noopener";
        playLink.textContent = "Play";

        const dlLink = document.createElement("a");
        dlLink.className = "btn btn-sm";
        dlLink.href = `/admin/cache/${encodeURIComponent(item.id)}/file`;
        dlLink.download = item.file_name || "";
        dlLink.textContent = "Download";

        const delBtn = document.createElement("button");
        delBtn.type = "button";
        delBtn.className = "btn btn-sm btn-danger";
        delBtn.textContent = "Delete";
        delBtn.addEventListener("click", async () => {
          if (!confirm(`Delete "${item.name}" from cache?`)) return;
          delBtn.disabled = true;
          try {
            const res = await fetch(
              `/admin/cache/${encodeURIComponent(item.id)}/delete`,
              { method: "POST" }
            );
            if (redirectIfLoggedOut(res)) return;
            if (!res.ok) throw new Error("delete failed");
            const data = await res.json();
            cachedItems = cachedItems.filter((x) => x.id !== item.id);
            selectedIds.delete(item.id);
            if (typeof data.used_mb === "string") cacheUsedMb = data.used_mb;
            if (typeof data.entries === "number") cacheEntries = data.entries;
            syncUsageText();
            syncFilterOptions();
            renderCacheList();
          } catch {
            delBtn.disabled = false;
            alert("Couldn't delete cached track");
          }
        });

        actions.append(playLink, dlLink, delBtn);
        row.append(checkbox, coverLink, info, actions);
        frag.append(row);
      }
      listEl.append(frag);
      syncSelectionUi();
    };

    const loadCacheList = async () => {
      if (listEl) {
        listEl.replaceChildren();
        listEl.hidden = true;
      }
      if (emptyEl) {
        emptyEl.textContent = "Loading...";
        emptyEl.hidden = false;
      }
      if (summaryEl) summaryEl.textContent = "";
      try {
        const res = await fetch("/admin/cache/list");
        if (redirectIfLoggedOut(res)) return;
        if (!res.ok) throw new Error("load failed");
        const data = await res.json();
        cachedItems = Array.isArray(data.items) ? data.items : [];
        const liveIds = new Set(cachedItems.map((x) => x.id));
        for (const id of [...selectedIds]) {
          if (!liveIds.has(id)) selectedIds.delete(id);
        }
        cacheUsedMb = typeof data.used_mb === "string" ? data.used_mb : "0.0";
        cacheEntries = typeof data.entries === "number" ? data.entries : cachedItems.length;
        syncUsageText();
        syncFilterOptions();
        renderCacheList();
      } catch {
        if (emptyEl) {
          emptyEl.textContent = "Couldn't load cached tracks.";
          emptyEl.hidden = false;
        }
      }
    };

    if (selectAllBtn) {
      selectAllBtn.addEventListener("click", () => {
        if (visibleItems.length === 0) return;
        const allSelected = visibleItems.every((item) => selectedIds.has(item.id));
        if (allSelected) {
          for (const item of visibleItems) selectedIds.delete(item.id);
        } else {
          for (const item of visibleItems) selectedIds.add(item.id);
        }
        anchorId = null;
        syncSelectionUi();
      });
    }

    if (bulkDownloadBtn) {
      bulkDownloadBtn.addEventListener("click", () => {
        const ids = cachedItems.filter((x) => selectedIds.has(x.id)).map((x) => x.id);
        if (ids.length === 0) return;
        if (ids.length === 1) {
          const item = cachedItems.find((x) => x.id === ids[0]);
          const a = document.createElement("a");
          a.href = `/admin/cache/${encodeURIComponent(ids[0])}/file`;
          a.download = (item && item.file_name) || "";
          a.hidden = true;
          document.body.append(a);
          a.click();
          a.remove();
          return;
        }
        if (bulkDownloadForm && bulkDownloadIdsInput) {
          bulkDownloadIdsInput.value = ids.join("\n");
          bulkDownloadForm.submit();
        }
      });
    }

    if (bulkDeleteBtn) {
      bulkDeleteBtn.addEventListener("click", async () => {
        const ids = cachedItems.filter((x) => selectedIds.has(x.id)).map((x) => x.id);
        if (ids.length === 0) return;
        const msg =
          ids.length === 1
            ? `Delete "${cachedItems.find((x) => x.id === ids[0])?.name || "track"}" from cache?`
            : `Delete ${ids.length} cached tracks?`;
        if (!confirm(msg)) return;
        bulkDeleteBtn.disabled = true;
        try {
          const res = await fetch("/admin/cache/delete-many", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ ids }),
          });
          if (redirectIfLoggedOut(res)) return;
          if (!res.ok) throw new Error("delete failed");
          const data = await res.json();
          const removedSet = new Set(ids);
          cachedItems = cachedItems.filter((x) => !removedSet.has(x.id));
          for (const id of ids) selectedIds.delete(id);
          anchorId = null;
          if (typeof data.used_mb === "string") cacheUsedMb = data.used_mb;
          if (typeof data.entries === "number") cacheEntries = data.entries;
          syncUsageText();
          syncFilterOptions();
          renderCacheList();
        } catch {
          syncSelectionUi();
          alert("Couldn't delete selected tracks");
        }
      });
    }

    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && selectedIds.size > 0 && !panelCache.hidden) {
        clearSelection();
      }
    });

    openCachePanel = () => {
      clearSelection();
      if (searchInput) searchInput.value = "";
      if (qualityFilter) qualityFilter.value = "";
      if (sourceFilter) sourceFilter.value = "";
      loadCacheList();
      if (searchInput) searchInput.focus();
    };

    window.addEventListener("scroll", hidePreview, { passive: true });
    if (searchInput) {
      searchInput.addEventListener("input", renderCacheList);
    }
    if (qualityFilter) {
      qualityFilter.addEventListener("change", renderCacheList);
    }
    if (sourceFilter) {
      sourceFilter.addEventListener("change", renderCacheList);
    }
  }

  const disconnectForm = document.getElementById("disconnect-form");
  if (disconnectForm) {
    disconnectForm.addEventListener("submit", (e) => {
      if (!confirm("Disconnect togewire from your Spotify account?")) e.preventDefault();
    });
  }

  const manualDownloadForm = document.getElementById("manual-download-form");
  const tabDashboard = document.getElementById("tab-dashboard");
  const tabDownload = document.getElementById("tab-download");
  const panelDashboard = document.getElementById("panel-dashboard");
  const panelDownload = document.getElementById("panel-download");
  const cacheBackBtn = document.getElementById("cache-back-btn");
  let refreshCurrentJob = null;
  if (tabDashboard && tabDownload && panelDashboard && panelDownload) {
    const setActiveTab = (tab, updateHash = true) => {
      const target = tab === "cache" && panelCache ? "cache" : tab === "download" ? "download" : "dashboard";
      const isDashboard = target === "dashboard";
      const isDownload = target === "download";
      const isCache = target === "cache";
      if (!isCache) leaveCachePanel();
      tabDashboard.classList.toggle("is-active", isDashboard);
      tabDashboard.setAttribute("aria-selected", String(isDashboard));
      tabDownload.classList.toggle("is-active", isDownload);
      tabDownload.setAttribute("aria-selected", String(isDownload));
      panelDashboard.hidden = !isDashboard;
      panelDownload.hidden = !isDownload;
      if (panelCache) panelCache.hidden = !isCache;
      if (updateHash) {
        const nextHash = isDownload ? "#download" : isCache ? "#cache" : "";
        if (window.location.hash !== nextHash) {
          window.history.replaceState({}, document.title, window.location.pathname + nextHash);
        }
      }
      if (isDownload) {
        if (refreshCurrentJob) refreshCurrentJob();
        document.getElementById("manual-download-url")?.focus();
      } else if (isCache && openCachePanel) {
        openCachePanel();
      }
    };
    tabDashboard.addEventListener("click", () => setActiveTab("dashboard"));
    tabDownload.addEventListener("click", () => setActiveTab("download"));
    if (viewCacheBtn && panelCache) {
      viewCacheBtn.addEventListener("click", () => setActiveTab("cache"));
    }
    if (cacheBackBtn) {
      cacheBackBtn.addEventListener("click", () => setActiveTab("dashboard"));
    }
    if (window.location.hash === "#download") {
      setActiveTab("download", false);
    } else if (window.location.hash === "#cache" && panelCache) {
      setActiveTab("cache", false);
    }
  }
  if (manualDownloadForm) {
    const urlInput = document.getElementById("manual-download-url");
    const sourceGroup = document.getElementById("manual-download-source-group");
    const qualityGroup = document.getElementById("manual-download-quality-group");
    const qualitySelect = document.getElementById("manual-download-quality");
    const normalizeInput = document.getElementById("manual-download-normalize");
    const downloadBtn = document.getElementById("manual-download-btn");
    const errorEl = document.getElementById("manual-download-error");
    const listEl = document.getElementById("manual-download-list");

    const isYoutubeUrl = (val) =>
      /(?:^|\/\/)(?:(?:www|m|music)\.)?(?:youtube\.com|youtu\.be)\//i.test(val.trim());
    const syncManualDownloadOptions = () => {
      const ytLink = urlInput ? isYoutubeUrl(urlInput.value) : false;
      if (sourceGroup) {
        sourceGroup.hidden = ytLink;
      }
      if (qualityGroup) {
        const checkedSource = manualDownloadForm.querySelector('input[name="source"]:checked');
        const usingYtdlp = ytLink || !checkedSource || checkedSource.value === "ytdlp";
        qualityGroup.hidden = usingYtdlp;
      }
    };
    if (urlInput) {
      urlInput.addEventListener("input", syncManualDownloadOptions);
    }
    manualDownloadForm.addEventListener("change", syncManualDownloadOptions);
    syncManualDownloadOptions();

    const startedJobIds = new Set();
    const autoSavedJobIds = new Set();
    let currentJobs = [];
    let lastRenderedJson = "";
    let pollTimer = null;
    let submitting = false;

    const stopPolling = () => {
      if (pollTimer) {
        clearTimeout(pollTimer);
        pollTimer = null;
      }
    };

    const schedulePoll = (delayMs) => {
      stopPolling();
      pollTimer = setTimeout(() => {
        pollTimer = null;
        pollQueue();
      }, delayMs);
    };

    const setHeaderLabel = (suffix) => {
      if (!tabDownload) return;
      tabDownload.textContent = suffix
        ? `Direct download (${suffix})`
        : "Direct download";
    };

    const setFormError = (msg) => {
      if (!errorEl) return;
      errorEl.textContent = msg || "";
      errorEl.hidden = !msg;
    };

    const hasPendingWork = (jobs) =>
      jobs.some((j) => j.status === "running" || j.status === "paused" || j.status === "queued");

    const syncSubmitButton = () => {
      if (!downloadBtn) return;
      downloadBtn.disabled = submitting;
      if (submitting) {
        downloadBtn.textContent = "Adding...";
      } else {
        downloadBtn.textContent = hasPendingWork(currentJobs) ? "Add to queue" : "Start download";
      }
    };

    const triggerFileSave = (jobId) => {
      const a = document.createElement("a");
      a.href = `/admin/download/${encodeURIComponent(jobId)}/file`;
      a.download = "";
      a.hidden = true;
      document.body.append(a);
      a.click();
      a.remove();
    };

    const updateTabBadge = (jobs) => {
      const active = jobs.find((j) => j.status === "running" || j.status === "paused");
      const queuedCount = jobs.filter((j) => j.status === "queued").length;
      const readyCount = jobs.filter((j) => j.status === "ready").length;
      const errorCount = jobs.filter((j) => j.status === "error").length;

      if (active) {
        let base;
        if (active.status === "paused") {
          base = "Paused";
        } else if (active.total > 1) {
          const step = Math.min(active.done + 1, active.total);
          base = `${step}/${active.total}`;
        } else {
          base = "1/1";
        }
        setHeaderLabel(queuedCount > 0 ? `${base}, +${queuedCount}` : base);
      } else if (queuedCount > 0) {
        setHeaderLabel(`${queuedCount} queued`);
      } else if (readyCount > 0) {
        setHeaderLabel(readyCount === 1 ? "Ready" : `${readyCount} ready`);
      } else if (errorCount > 0) {
        setHeaderLabel("Failed");
      } else {
        setHeaderLabel("");
      }
    };

    const postJobAction = async (jobId, action) => {
      try {
        const res = await fetch(
          `/admin/download/${encodeURIComponent(jobId)}/${action}`,
          { method: "POST" }
        );
        if (redirectIfLoggedOut(res)) return;
        if (res.ok) {
          const data = await res.json().catch(() => null);
          if (data && Array.isArray(data.jobs)) {
            applyJobs(data.jobs);
          } else {
            pollQueue();
          }
        }
      } catch {
        /* ignore */
      }
    };

    const renderJobItem = (job, queuePos) => {
      const item = document.createElement("div");
      item.className = "manual-download-item";

      if (job.title) {
        const title = document.createElement("p");
        title.className = "manual-download-title";
        title.textContent = job.title;
        item.append(title);
      } else if (
        job.input &&
        (job.status === "queued" ||
          job.status === "error" ||
          ((job.status === "running" || job.status === "paused") && !job.current))
      ) {
        const inp = document.createElement("p");
        inp.className = "manual-download-input";
        inp.textContent = job.input;
        inp.title = job.input;
        item.append(inp);
      }

      if (job.details) {
        const meta = document.createElement("p");
        meta.className = "manual-download-meta";
        meta.textContent = job.details;
        item.append(meta);
      }

      const status = document.createElement("p");
      status.className =
        job.status === "error" ? "hint manual-download-status-error" : "hint";
      if (job.status === "queued") {
        status.textContent = queuePos > 0 ? `Queued (#${queuePos})` : "Queued";
      } else if (job.status === "running" || job.status === "paused") {
        const verb = job.stopping
          ? "Finishing"
          : job.status === "paused"
            ? "Paused"
            : "Downloading";
        if (job.total > 1) {
          const step = Math.min(job.done + 1, job.total);
          status.textContent = `${verb} (${step}/${job.total}): ${job.current || "..."}`;
        } else {
          status.textContent = job.current ? `${verb}: ${job.current}` : "Resolving link...";
        }
      } else if (job.status === "ready") {
        const skipNote = job.skipped > 0 ? ` (${job.skipped} skipped)` : "";
        status.textContent = `Ready: ${job.filename || "download"}${skipNote}`;
      } else if (job.status === "error") {
        status.textContent = job.error || "Download failed";
      }
      item.append(status);

      const actions = document.createElement("div");
      actions.className = "actions";

      if (job.status === "queued") {
        const removeBtn = document.createElement("button");
        removeBtn.type = "button";
        removeBtn.className = "btn btn-sm btn-danger";
        removeBtn.textContent = "Remove";
        removeBtn.addEventListener("click", () => postJobAction(job.id, "cancel"));
        actions.append(removeBtn);
      } else if (job.status === "running" || job.status === "paused") {
        const paused = job.status === "paused";
        const stopping = !!job.stopping;
        if ((job.total > 1 || paused) && !stopping) {
          const pauseBtn = document.createElement("button");
          pauseBtn.type = "button";
          pauseBtn.className = "btn btn-sm";
          pauseBtn.textContent = paused ? "Resume" : "Pause";
          pauseBtn.addEventListener("click", () =>
            postJobAction(job.id, paused ? "resume" : "pause")
          );
          actions.append(pauseBtn);
        }
        if (job.total > 1) {
          const stopBtn = document.createElement("button");
          stopBtn.type = "button";
          stopBtn.className = "btn btn-sm";
          stopBtn.disabled = stopping;
          stopBtn.textContent = stopping ? "Stopping..." : "Stop";
          stopBtn.addEventListener("click", () => postJobAction(job.id, "stop"));
          actions.append(stopBtn);
        }
        const cancelBtn = document.createElement("button");
        cancelBtn.type = "button";
        cancelBtn.className = "btn btn-sm btn-danger";
        cancelBtn.textContent = "Cancel";
        cancelBtn.addEventListener("click", () => {
          if (
            confirm("Cancel this download and delete its temporary files from the server?")
          ) {
            postJobAction(job.id, "cancel");
          }
        });
        actions.append(cancelBtn);
      } else if (job.status === "ready") {
        const saveBtn = document.createElement("a");
        saveBtn.className = "btn btn-sm";
        saveBtn.href = `/admin/download/${encodeURIComponent(job.id)}/file`;
        saveBtn.download = "";
        saveBtn.textContent = "Save to device";
        actions.append(saveBtn);

        const dismissBtn = document.createElement("button");
        dismissBtn.type = "button";
        dismissBtn.className = "btn btn-sm btn-danger";
        dismissBtn.textContent = "Dismiss";
        dismissBtn.addEventListener("click", () => {
          if (
            confirm("Dismiss this download and delete its temporary files from the server?")
          ) {
            postJobAction(job.id, "cancel");
          }
        });
        actions.append(dismissBtn);
      } else if (job.status === "error") {
        const dismissBtn = document.createElement("button");
        dismissBtn.type = "button";
        dismissBtn.className = "btn btn-sm btn-danger";
        dismissBtn.textContent = "Dismiss";
        dismissBtn.addEventListener("click", () => postJobAction(job.id, "cancel"));
        actions.append(dismissBtn);
      }

      if (actions.childElementCount > 0) {
        item.append(actions);
      }
      return item;
    };

    const applyJobs = (jobs) => {
      currentJobs = Array.isArray(jobs) ? jobs : [];
      syncSubmitButton();
      updateTabBadge(currentJobs);

      if (listEl) {
        const nextJson = JSON.stringify(currentJobs);
        if (nextJson !== lastRenderedJson) {
          lastRenderedJson = nextJson;
          listEl.replaceChildren();
          let queuedIdx = 0;
          for (const job of currentJobs) {
            if (job.status === "queued") queuedIdx += 1;
            listEl.append(renderJobItem(job, job.status === "queued" ? queuedIdx : 0));
          }
          listEl.hidden = currentJobs.length === 0;
        }
        for (const job of currentJobs) {
          if (
            job.status === "ready" &&
            startedJobIds.has(job.id) &&
            !autoSavedJobIds.has(job.id)
          ) {
            autoSavedJobIds.add(job.id);
            triggerFileSave(job.id);
          }
        }
      }

      if (hasPendingWork(currentJobs)) {
        schedulePoll(1000);
      } else {
        stopPolling();
      }
    };

    const pollQueue = async () => {
      try {
        const res = await fetch("/admin/download");
        if (redirectIfLoggedOut(res)) return;
        if (!res.ok) return;
        const data = await res.json().catch(() => null);
        if (data && Array.isArray(data.jobs)) {
          applyJobs(data.jobs);
        }
      } catch {
        if (hasPendingWork(currentJobs)) {
          schedulePoll(1500);
        }
      }
    };

    // Reattach to queued, active, ready, or failed downloads on load or when opening the tab.
    refreshCurrentJob = () => {
      pollQueue();
    };
    refreshCurrentJob();

    manualDownloadForm.addEventListener("submit", async (e) => {
      e.preventDefault();
      if (!manualDownloadForm.reportValidity()) return;

      const url = urlInput ? urlInput.value.trim() : "";
      const quality = qualitySelect ? qualitySelect.value : "max";
      const sourceInput = manualDownloadForm.querySelector('input[name="source"]:checked');
      const source =
        sourceGroup && !sourceGroup.hidden && sourceInput ? sourceInput.value : undefined;
      const normalize = normalizeInput ? normalizeInput.checked : false;

      submitting = true;
      setFormError("");
      syncSubmitButton();

      try {
        const res = await fetch("/admin/download", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ url, quality, source, normalize }),
        });
        if (redirectIfLoggedOut(res)) return;
        const data = await res.json().catch(() => ({}));
        if (!res.ok || !data.id) {
          throw new Error(data.error || "Could not start download");
        }
        startedJobIds.add(data.id);
        if (urlInput) {
          urlInput.value = "";
          syncManualDownloadOptions();
          urlInput.focus();
        }
        if (Array.isArray(data.jobs)) {
          applyJobs(data.jobs);
        } else {
          pollQueue();
        }
      } catch (err) {
        setFormError(err.message || "Could not start download");
      } finally {
        submitting = false;
        syncSubmitButton();
      }
    });
  }

  const startPairBtn = document.getElementById("start-pair-btn");
  if (startPairBtn) {
    startPairBtn.addEventListener("click", async () => {
      startPairBtn.disabled = true;
      startPairBtn.textContent = "Opening Spotify...";

      // Open new tab early in user gesture context to avoid popup blocker
      const pairWindow = window.open("about:blank", "_blank");
      if (pairWindow) pairWindow.opener = null;

      try {
        const res = await fetch("/admin/pair/start", { method: "POST" });
        if (redirectIfLoggedOut(res)) {
          if (pairWindow) pairWindow.close();
          return;
        }
        if (!res.ok) {
          const errData = await res.json().catch(() => ({}));
          throw new Error(errData.error || "Failed to start pairing");
        }
        const data = await res.json();
        const linkUrl = data.verification_uri_complete ||
          `${data.verification_uri || "https://spotify.com/pair"}?code=${encodeURIComponent(data.user_code)}`;
        if (typeof linkUrl !== "string" || !linkUrl.startsWith("https://")) {
          throw new Error("Invalid verification URL from Spotify");
        }

        if (pairWindow) {
          pairWindow.location.href = linkUrl;
        } else {
          window.open(linkUrl, "_blank");
        }

        startPairBtn.textContent = "Waiting for Spotify...";

        let intervalMs = Math.max(data.interval || 5, 3) * 1000;
        let pollFailures = 0;
        const deviceCode = data.device_code;

        async function poll() {
          try {
            const pollRes = await fetch("/admin/pair/poll", {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify({ device_code: deviceCode }),
            });
            if (redirectIfLoggedOut(pollRes)) return;
            if (!pollRes.ok) throw new Error("Poll request failed");
            pollFailures = 0;
            const pollData = await pollRes.json();
            if (pollData.status === "paired") {
              window.location.href = "/admin?n=paired";
              return;
            }
            if (pollData.status === "error") {
              alert("Pairing error: " + (pollData.message || "Unknown error"));
              startPairBtn.disabled = false;
              startPairBtn.textContent = "Pair Spotify";
              return;
            }
            // RFC 8628: slow_down means add 5s to the interval from now on.
            if (pollData.status === "slow_down") intervalMs += 5000;
            setTimeout(poll, intervalMs);
          } catch {
            pollFailures += 1;
            if (pollFailures >= 5) {
              alert("Lost connection to togewire while waiting for Spotify pairing.");
              startPairBtn.disabled = false;
              startPairBtn.textContent = "Pair Spotify";
              return;
            }
            setTimeout(poll, intervalMs);
          }
        }

        setTimeout(poll, intervalMs);
      } catch (e) {
        if (pairWindow) pairWindow.close();
        alert("Could not start pairing: " + e.message);
        startPairBtn.disabled = false;
        startPairBtn.textContent = "Pair Spotify";
      }
    });
  }

  const toggle = document.getElementById("player-toggle");
  const playerNav = document.getElementById("player-nav");
  const switchLabel = document.getElementById("player-toggle-label");
  if (!toggle || !playerNav || toggle.disabled) return;

  let liveWs = null;
  let liveReconnectTimer = null;
  let liveReconnectDelay = 1000;

  function disconnectLiveWs() {
    if (liveReconnectTimer) {
      clearTimeout(liveReconnectTimer);
      liveReconnectTimer = null;
    }
    if (liveWs) {
      liveWs.onclose = null;
      liveWs.onmessage = null;
      liveWs.close();
      liveWs = null;
    }
    playerNav.classList.remove("is-live");
  }

  function connectLiveWs() {
    if (!toggle.checked || liveWs) return;
    if (liveReconnectTimer) {
      clearTimeout(liveReconnectTimer);
      liveReconnectTimer = null;
    }
    try {
      const proto = location.protocol === "https:" ? "wss:" : "ws:";
      const ws = new WebSocket(`${proto}//${location.host}/ws`);
      liveWs = ws;
      ws.onopen = () => {
        liveReconnectDelay = 1000;
      };
      ws.onmessage = (e) => {
        try {
          const msg = JSON.parse(e.data);
          if (msg.type === "track_update") {
            playerNav.classList.toggle("is-live", !!(msg.data && msg.data.isPlaying));
          }
        } catch {
          /* ignore */
        }
      };
      ws.onclose = () => {
        if (liveWs === ws) liveWs = null;
        playerNav.classList.remove("is-live");
        if (!toggle.checked) return;
        liveReconnectTimer = setTimeout(() => {
          liveReconnectTimer = null;
          liveReconnectDelay = Math.min(liveReconnectDelay * 2, 5000);
          connectLiveWs();
        }, liveReconnectDelay);
      };
    } catch {
      /* ignore */
    }
  }

  const lowDataGroup = document.getElementById("low-data-group");
  function setPlayerUi(enabled) {
    toggle.checked = enabled;
    playerNav.classList.toggle("is-on", enabled);
    if (!enabled) disconnectLiveWs();
    if (enabled) playerNav.removeAttribute("title");
    else playerNav.setAttribute("title", "Turn the player on first");
    if (switchLabel) switchLabel.textContent = enabled ? "On" : "Off";
    if (lowDataGroup) lowDataGroup.hidden = !enabled;
  }

  setPlayerUi(toggle.checked);
  if (toggle.checked) connectLiveWs();
  window.addEventListener("pagehide", disconnectLiveWs);

  let busy = false;
  toggle.addEventListener("change", async () => {
    if (busy) return;
    busy = true;
    const enabled = toggle.checked;
    setPlayerUi(enabled);
    try {
      const res = await fetch("/admin/player", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ enabled }),
      });
      if (redirectIfLoggedOut(res)) return;
      if (!res.ok) throw new Error("save failed");
      const data = await res.json();
      setPlayerUi(!!data.enabled);
      if (data.enabled) connectLiveWs();
    } catch {
      setPlayerUi(!enabled);
      if (!enabled) connectLiveWs();
      alert("Couldn't turn the player " + (enabled ? "on" : "off"));
    } finally {
      busy = false;
    }
  });
})();
