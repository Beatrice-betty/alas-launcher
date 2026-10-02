// Embedded after the splash's i18n and text helpers. One frame clock drives both
// progress bars and the decoration so the displayed percentage matches the fill.
(() => {
  const duration = 250;
  const elements = Object.fromEntries([
    'badge', 'badge-text', 'spinner', 'error-dot', 'progress-fill',
    'progress-pct', 'progress-head', 'uv-progress-container',
    'uv-progress-fill', 'uv-progress-pct', 'uv-progress-detail',
    'progress-meta', 'splash-actions', 'tip-text', 'title', 'detail',
  ].map(id => [id, document.getElementById(id)]));
  const track = elements['progress-fill'].parentElement;
  const motion = window.matchMedia('(prefers-reduced-motion: reduce)');
  const createChannel = (fill, label) => ({
    fill, label, rendered: 0, target: 0, from: 0, startedAt: 0,
    moving: false, drawn: null, percentage: null,
  });
  const main = createChannel(elements['progress-fill'], elements['progress-pct']);
  const uv = createChannel(elements['uv-progress-fill'], elements['uv-progress-pct']);
  let lastSequence = -1;
  let frame = null;
  let uvVisible = false;
  let uvTaskId = null;
  let errorState = null;
  let trackWidth = 0;
  let headTransform = '';
  let frozen = false;

  const text = (element, value) => {
    if (element.textContent !== value) element.textContent = value;
  };
  const style = (element, property, value) => {
    if (element.style[property] !== value) element.style[property] = value;
  };
  const className = (element, value) => {
    if (element.className !== value) element.className = value;
  };
  const confirmedProgress = value => typeof value === 'number' && Number.isFinite(value)
    ? Math.max(0, Math.min(100, value)) : null;

  function drawHead() {
    const edge = Math.min(30, trackWidth / 2);
    const position = Math.max(edge, Math.min(trackWidth - edge, trackWidth * main.rendered / 100));
    const transform = `translate3d(${position}px, -80%, 0) translateX(-50%)`;
    if (transform !== headTransform) {
      elements['progress-head'].style.transform = transform;
      headTransform = transform;
    }
  }

  function measureTrack() {
    const width = track.getBoundingClientRect().width;
    trackWidth = Number.isFinite(width) ? Math.max(0, width) : 0;
    drawHead();
  }

  function draw(channel) {
    if (channel.drawn !== channel.rendered) {
      channel.fill.style.transform = `scaleX(${channel.rendered / 100})`;
      channel.drawn = channel.rendered;
      if (channel === main) drawHead();
    }
    const percentage = Math.floor(channel.rendered);
    if (channel.percentage !== percentage) {
      text(channel.label, `${percentage}%`);
      channel.fill.setAttribute('aria-valuenow', String(percentage));
      channel.percentage = percentage;
    }
  }

  function advance(channel, now) {
    if (!channel.moving) return;
    const elapsed = Math.max(0, Math.min(1, (now - channel.startedAt) / duration));
    channel.rendered = Math.max(channel.rendered,
      Math.min(channel.target, channel.from + (channel.target - channel.from) * elapsed));
    channel.moving = elapsed < 1;
    draw(channel);
  }

  function finish(channel) {
    channel.rendered = channel.target;
    channel.moving = false;
    draw(channel);
  }

  function cancelFrame() {
    if (frame !== null) {
      window.cancelAnimationFrame(frame);
      frame = null;
    }
  }

  function scheduleFrame() {
    if (frame !== null || motion.matches || document.hidden || frozen) return;
    if (main.moving || (uvVisible && uv.moving)) {
      frame = window.requestAnimationFrame(animate);
    }
  }

  function animate(now) {
    frame = null;
    advance(main, now);
    if (uvVisible) advance(uv, now);
    scheduleFrame();
  }

  function setTarget(channel, value, now, immediate) {
    if (value !== null) {
      const target = Math.max(channel.target, value);
      if (target !== channel.target) {
        channel.from = channel.rendered;
        channel.startedAt = now;
        channel.target = target;
        channel.moving = channel.rendered < target;
      }
    }
    if (immediate || channel.target === 100) finish(channel);
  }

  function setUvVisible(visible) {
    if (visible === uvVisible) return;
    uvVisible = visible;
    elements['uv-progress-container'].classList.toggle('is-visible', visible);
    elements['uv-progress-container'].setAttribute('aria-hidden', String(!visible));
    if (!visible) {
      uvTaskId = null;
      resetUv();
    }
  }

  function resetUv() {
    uv.rendered = uv.target = uv.from = 0;
    uv.moving = false;
    draw(uv);
  }

  function setError(error) {
    if (error === errorState) return;
    errorState = error;
    document.body.classList.toggle('error-state', error);
    className(elements.badge, error ? 'status-badge status-badge-err' : 'status-badge');
    style(elements.spinner, 'display', error ? 'none' : 'block');
    style(elements['error-dot'], 'display', error ? 'flex' : 'none');
    className(main.fill, error ? 'progress-bar-fill progress-bar-fill-error' : 'progress-bar-fill');
    className(elements['splash-actions'], error ? 'splash-actions splash-actions-err' : 'splash-actions');
  }

  window.__ALAS_SPLASH_UPDATE = payload => {
    if (!payload || typeof payload !== 'object'
      || !Number.isSafeInteger(payload.sequence) || payload.sequence < 0
      || payload.sequence <= lastSequence || (frozen && payload.terminal !== true)) return;
    lastSequence = payload.sequence;
    const now = performance.now();
    advance(main, now);
    if (uvVisible) advance(uv, now);

    const error = payload.is_error === true;
    const progress = confirmedProgress(payload.progress);
    const terminal = payload.terminal === true;
    const completed = progress === 100;
    const immediate = error || terminal || completed || motion.matches || document.hidden;
    const subtitle = splitSubtitle(payload.subtitle);
    text(elements['badge-text'], error ? i18n.errorBadge : subtitle.status);
    text(elements['tip-text'], subtitle.tip);
    text(elements.title, String(payload.title || i18n.starting));
    text(elements.detail, normalizeDetail(payload.detail));
    text(elements['progress-meta'], error ? i18n.initStopped : i18n.progressMetaReady);
    setError(error);
    setTarget(main, progress, now, immediate);

    const uvState = payload.uv_progress;
    const uvProgress = uvState && confirmedProgress(uvState.progress);
    const taskId = uvState && (uvState.task_id === undefined ? 0 : uvState.task_id);
    if (error || terminal || completed || !uvState) {
      setUvVisible(false);
    } else if (uvProgress !== null && Number.isSafeInteger(taskId) && taskId >= 0) {
      if (taskId !== uvTaskId) {
        resetUv();
        uvTaskId = taskId;
      }
      setUvVisible(true);
      text(elements['uv-progress-detail'], String(uvState.detail || ''));
      setTarget(uv, uvProgress, now, immediate);
    }
    // A malformed uv value keeps the last confirmed state; it must not create
    // a hidden/show cycle that would reset a running task's high-water mark.
    if (terminal) frozen = true;
    if (immediate) cancelFrame();
    scheduleFrame();
  };

  function settle() {
    cancelFrame();
    finish(main);
    if (uvVisible) finish(uv);
  }

  const motionChanged = () => {
    if (motion.matches) settle();
  };
  if (typeof motion.addEventListener === 'function') {
    motion.addEventListener('change', motionChanged);
  } else {
    motion.addListener(motionChanged);
  }
  document.addEventListener('visibilitychange', () => {
    settle();
    if (!document.hidden) measureTrack();
  });
  window.addEventListener('resize', measureTrack);
  if (typeof ResizeObserver === 'function') {
    const observer = new ResizeObserver(measureTrack);
    observer.observe(track);
  }
  draw(main);
  draw(uv);
  measureTrack();

  window.__ALAS_SPLASH_READY = true;
  Promise.resolve().then(() => {
    if (typeof invoke !== 'function') throw new Error('Tauri invoke is unavailable');
    return invoke('splash_ready');
  }).then(payload => {
    if (payload) window.__ALAS_SPLASH_UPDATE(payload);
  }).catch(error => console.error('Failed to synchronize splash progress', error));
})();
