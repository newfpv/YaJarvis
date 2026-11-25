<script>
  import { onMount } from "svelte";
  import { icon, toast } from "#helpers/core.js";
  import { speaker } from "#state/panelState.js";
  import {
    music,
    playMusicAction,
    setYoutubeLike,
    subscribeLikes,
    youtubeSearch,
  } from "#state/player.js";
  import qStyles from "../PlayerQueue/PlayerQueue.module.css";
  import styles from "./YouTubeSearch.module.css";

  let query = $state("");
  let lastQuery = $state("");
  let kind = $state("video");
  let sort = $state("relevance");
  let date = $state("");
  let duration = $state("");
  let live = $state(false);
  let items = $state.raw([]);
  let loading = $state(false);
  let error = $state("");
  let busy = $state(false);
  let likedSet = $state.raw(new Set());
  let searchSeq = 0;
  let searchCtrl = null;

  let pluginReady = $derived(Boolean($speaker?.youtube?.plugin));
  let youtubeSigned = $derived(
    Boolean($speaker?.youtube?.plugin && ($speaker?.youtube?.account?.signed_in || $speaker?.youtube?.set)),
  );
  let hasFilters = $derived(
    kind !== "video" || sort !== "relevance" || date !== "" || (kind !== "playlist" && (duration !== "" || live)),
  );

  onMount(() => {
    const unsubLikes = subscribeLikes((ids) => {
      likedSet = new Set(ids);
    });
    return () => {
      searchCtrl?.abort();
      unsubLikes();
    };
  });

  async function runSearch(raw = query) {
    const q = String(raw || "").trim();
    if (!q || !pluginReady) return;
    lastQuery = q;
    searchCtrl?.abort();
    const ctrl = new AbortController();
    searchCtrl = ctrl;
    const seq = ++searchSeq;
    loading = true;
    error = "";
    try {
      const data = await youtubeSearch(
        { q, type: kind, sort, date, duration, live },
        { signal: ctrl.signal },
      );
      if (seq !== searchSeq) return;
      items = data?.items || [];
    } catch (failure) {
      if (ctrl.signal.aborted || seq !== searchSeq) return;
      error = failure?.message || "Не удалось выполнить поиск";
    } finally {
      if (seq === searchSeq) loading = false;
    }
  }

  function submit(event) {
    event.preventDefault();
    void runSearch(query);
  }

  function onFilterChange() {
    const activeQuery = query.trim() || lastQuery;
    if (activeQuery) {
      void runSearch(activeQuery);
    }
  }

  function resetFilters() {
    kind = "video";
    sort = "relevance";
    date = "";
    duration = "";
    live = false;
    onFilterChange();
  }

  function clearSearch() {
    searchCtrl?.abort();
    query = "";
    lastQuery = "";
    items = [];
    error = "";
    loading = false;
  }

  async function playVideo(item) {
    if (!item?.ID || busy) return;
    busy = true;
    try {
      await playMusicAction({
        action: "id",
        query: item.ID,
        title: item.Title || "",
        author: item.Author || "",
      });
    } finally {
      busy = false;
    }
  }

  function queueVideo(item) {
    if (!item?.ID || busy) return;
    busy = true;
    void (async () => {
      try {
        const ok = await music({
          action: "queue",
          query: item.ID,
          title: item.Title || "",
          author: item.Author || "",
        });
        if (ok) toast("Добавлено в очередь");
      } finally {
        busy = false;
      }
    })();
  }

  async function playPlaylist(item, shuffle = false) {
    if (!item?.ID || busy) return;
    busy = true;
    try {
      await playMusicAction({ action: "playlist", id: item.ID, shuffle });
    } finally {
      busy = false;
    }
  }

  function toggleLike(item, event) {
    if (!item?.ID || busy || !youtubeSigned) return;
    const nextLiked = !likedSet.has(item.ID);
    busy = true;
    void (async () => {
      try {
        await setYoutubeLike(item.ID, nextLiked, item, event?.currentTarget);
      } finally {
        busy = false;
      }
    })();
  }

  function itemMeta(item) {
    if (item.kind === "playlist") {
      return [item.Author, item.Count || "Плейлист YouTube"].filter(Boolean).join(" · ");
    }
    return [item.Author, item.Views, item.Published].filter(Boolean).join(" · ");
  }
</script>

{#if pluginReady}
  <section class={["card", styles.section]} aria-labelledby="yt-search-title">
    <div class="card-head">
      <div>
        <h2 id="yt-search-title">Поиск на YouTube</h2>
        <span class="meta">отдельный поиск с фильтрами по типу, дате, длительности и эфирам</span>
      </div>
    </div>

    <form class={styles.form} onsubmit={submit}>
      <label class={styles.field}>
        {@html icon("search")}
        <input
          type="text"
          placeholder="Запрос на YouTube или ссылка на видео/плейлист…"
          autocomplete="off"
          aria-label="Поиск на YouTube"
          bind:value={query}
        />
        {#if query || lastQuery}
          <button
            class={["ib", styles.clearBtn]}
            type="button"
            aria-label="Очистить поиск"
            onclick={clearSearch}
          >
            {@html icon("x")}
          </button>
        {/if}
      </label>
      <button
        class={["btn primary", styles.searchBtn]}
        type="submit"
        disabled={loading || !query.trim()}
      >
        {loading ? "Ищу…" : "Найти"}
      </button>
    </form>

    <div class={styles.filters}>
      <div class="seg sm" role="group" aria-label="Тип результатов">
        {#each [["video", "Видео"], ["playlist", "Плейлисты"], ["all", "Всё"]] as [value, label] (value)}
          <button
            type="button"
            class={{ on: kind === value }}
            aria-pressed={kind === value}
            onclick={() => {
              if (kind === value) return;
              kind = value;
              if (kind === "playlist") {
                duration = "";
                live = false;
              }
              onFilterChange();
            }}
          >
            {label}
          </button>
        {/each}
      </div>

      <select
        class={styles.select}
        aria-label="Сортировка"
        bind:value={sort}
        onchange={onFilterChange}
      >
        <option value="relevance">По релевантности</option>
        <option value="date">По дате</option>
        <option value="views">По просмотрам</option>
        <option value="rating">По рейтингу</option>
      </select>

      <select
        class={styles.select}
        aria-label="Дата загрузки"
        bind:value={date}
        onchange={onFilterChange}
      >
        <option value="">За всё время</option>
        <option value="hour">За час</option>
        <option value="today">Сегодня</option>
        <option value="week">За неделю</option>
        <option value="month">За месяц</option>
        <option value="year">За год</option>
      </select>

      {#if kind !== "playlist"}
        <select
          class={styles.select}
          aria-label="Длительность"
          bind:value={duration}
          onchange={onFilterChange}
        >
          <option value="">Любая длина</option>
          <option value="short">До 4 мин</option>
          <option value="medium">4–20 мин</option>
          <option value="long">Более 20 мин</option>
        </select>

        <button
          type="button"
          class={[styles.toggleChip, { [styles.on]: live }]}
          aria-pressed={live}
          onclick={() => {
            live = !live;
            onFilterChange();
          }}
        >
          {@html icon("radio")}<span>Эфиры</span>
        </button>
      {/if}

      {#if hasFilters}
        <button
          type="button"
          class={["btn sm ghost", styles.resetBtn]}
          onclick={resetFilters}
        >
          Сбросить
        </button>
      {/if}
    </div>

    {#if error}
      <p class={styles.errorNote}>{error}</p>
    {/if}

    {#if loading && !items.length}
      <p class="empty">Ищу на YouTube…</p>
    {:else if items.length}
      <ul class={[qStyles.list, styles.scrollList]}>
        {#each items as item (`${item.kind}:${item.ID}`)}
          {@const liked = item.kind === "video" && likedSet.has(item.ID)}
          <li class={qStyles.row}>
            <button
              class={qStyles.track}
              type="button"
              disabled={busy || !item.ID}
              aria-label={item.kind === "playlist" ? `Включить плейлист «${item.Title}»` : `Включить «${item.Title}»`}
              onclick={() => (item.kind === "playlist" ? playPlaylist(item, false) : playVideo(item))}
            >
              <span class={styles.thumbWrap}>
                {#if item.Thumb}
                  <img class={styles.thumbImg} src={item.Thumb} alt="" loading="lazy" />
                {:else}
                  <span class={styles.thumbFallback}>{@html icon("music")}</span>
                {/if}
                {#if item.Duration || item.Count}
                  <span class={styles.durBadge}>{item.Duration || item.Count}</span>
                {/if}
              </span>
              <span class={qStyles.description}>
                <b>{item.Title || item.ID}</b>
                <span>{itemMeta(item)}</span>
              </span>
            </button>

            <div class={styles.rowActions}>
              {#if item.kind === "playlist"}
                <button
                  class={["ib", styles.actionIcon]}
                  type="button"
                  disabled={busy}
                  data-tip="Включить"
                  aria-label={`Включить плейлист «${item.Title}»`}
                  onclick={() => playPlaylist(item, false)}
                >
                  {@html icon("play", "solid")}
                </button>
                <button
                  class={["ib", styles.actionIcon]}
                  type="button"
                  disabled={busy}
                  data-tip="Вперемешку"
                  aria-label={`Включить плейлист «${item.Title}» вперемешку`}
                  onclick={() => playPlaylist(item, true)}
                >
                  {@html icon("shuffle")}
                </button>
              {:else}
                {#if youtubeSigned}
                  <button
                    class={["ib", styles.actionIcon, { liked }]}
                    type="button"
                    disabled={busy}
                    aria-pressed={liked}
                    data-tip={liked ? "Убрать лайк" : "Поставить лайк"}
                    aria-label={liked ? `Убрать лайк «${item.Title}»` : `Поставить лайк «${item.Title}»`}
                    onclick={(event) => toggleLike(item, event)}
                  >
                    {@html icon("heart")}
                  </button>
                {/if}
                <button
                  class={["ib", styles.actionIcon]}
                  type="button"
                  disabled={busy}
                  data-tip="В очередь"
                  aria-label={`Добавить в очередь «${item.Title}»`}
                  onclick={() => queueVideo(item)}
                >
                  {@html icon("plus")}
                </button>
              {/if}
            </div>
          </li>
        {/each}
      </ul>
    {:else if lastQuery && !loading && !error}
      <p class="empty">По запросу «{lastQuery}» ничего не найдено</p>
    {/if}
  </section>
{/if}
