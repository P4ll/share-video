import type Hls from 'hls.js'
import { playbackJumped } from './timing'

type PlaybackTarget = {
  position: number
  playing: boolean
  seek: 'none' | 'immediate' | 'drift'
  gesture: boolean
}

export type MediaPlayer = {
  position: () => number
  duration: () => number
  sync: (target: PlaybackTarget) => void
}
type MediaEvents = {
  ready: (player: MediaPlayer) => void
  started: () => void
  playbackChanged: (paused: boolean) => void
  canSeek: () => boolean
  seeked: (positionSec: number) => boolean
  blocked: () => void
  failed: (message: string) => void
}

type YouTubePlayer = {
  getCurrentTime: () => number
  getDuration: () => number
  getPlayerState: () => number
  getPlaybackRate: () => number
  seekTo: (seconds: number, allowSeekAhead: boolean) => void
  playVideo: () => void
  pauseVideo: () => void
  mute: () => void
  unMute: () => void
  destroy: () => void
}
type YouTubeEvent = { target: YouTubePlayer }
type YouTubeAPI = {
  Player: new (element: HTMLElement, options: { events: {
    onReady: (event: YouTubeEvent) => void
    onStateChange: (event: YouTubeEvent & { data: number }) => void
    onAutoplayBlocked: (event: YouTubeEvent) => void
    onError: (event: YouTubeEvent & { data: number }) => void
  } }) => YouTubePlayer
}

declare global {
  interface Window {
    YT?: YouTubeAPI
    onYouTubeIframeAPIReady?: () => void
  }
}

let youtubeAPI: Promise<YouTubeAPI> | undefined
function loadYoutubeAPI(): Promise<YouTubeAPI> {
  if (window.YT?.Player) return Promise.resolve(window.YT)
  if (youtubeAPI) return youtubeAPI
  youtubeAPI = new Promise((resolve, reject) => {
    const script = document.createElement('script')
    script.src = 'https://www.youtube.com/iframe_api'
    script.referrerPolicy = 'strict-origin'
    script.async = true
    function failed() {
      window.clearTimeout(timeout)
      delete window.onYouTubeIframeAPIReady
      youtubeAPI = undefined
      script.remove()
      reject(new Error('Не удалось загрузить плеер YouTube. Проверьте доступ к YouTube.'))
    }
    const timeout = window.setTimeout(failed, 12_000)
    window.onYouTubeIframeAPIReady = () => {
      if (!window.YT?.Player) return failed()
      window.clearTimeout(timeout)
      delete window.onYouTubeIframeAPIReady
      resolve(window.YT)
    }
    script.onerror = failed
    document.head.append(script)
  })
  return youtubeAPI
}

export function mountYouTube(container: HTMLElement, videoId: string, events: MediaEvents): () => void {
  let disposed = false
  let player: YouTubePlayer | undefined
  let playTimer = 0
  let priming = false
  let reportedPlaying = false
  let seekTimer = 0
  let sample: { position: number; at: number; rate: number } | undefined
  let roomSeek: { position: number; until: number } | undefined
  const iframe = document.createElement('iframe')

  // shortcut: IFrame API has no seek event; jumps under 0.75 s are ambiguous, use the room field for exact seeks.
  function observeSeek(target: YouTubePlayer): boolean {
    const state = target.getPlayerState()
    if (!events.canSeek() || priming || (state !== 1 && state !== 2 && state !== 3)) {
      sample = undefined
      return false
    }
    const position = target.getCurrentTime()
    if (!Number.isFinite(position)) return false
    const at = performance.now()
    const rate = target.getPlaybackRate() || 1
    const previous = sample
    sample = { position, at, rate }
    if (roomSeek) {
      if (Math.abs(position - roomSeek.position) < 0.75 || at >= roomSeek.until) roomSeek = undefined
      return false
    }
    return !!previous && playbackJumped(previous.position, position, (at - previous.at) / 1000, Math.max(previous.rate, rate))
      && events.seeked(position)
  }

  function resetPlayTimer() {
    window.clearTimeout(playTimer)
    playTimer = 0
  }

  function autoplayBlocked() {
    if (disposed) return
    resetPlayTimer()
    if (priming) {
      priming = false
      player?.unMute()
    }
    events.blocked()
  }

  void loadYoutubeAPI().then((YT) => {
    if (disposed) return
    const url = new URL(`https://www.youtube.com/embed/${encodeURIComponent(videoId)}`)
    url.search = new URLSearchParams({
      enablejsapi: '1', origin: location.origin, playsinline: '1', rel: '0',
    }).toString()
    iframe.src = url.href
    iframe.title = 'Встроенный плеер YouTube'
    iframe.referrerPolicy = 'strict-origin'
    iframe.allow = 'autoplay; encrypted-media; fullscreen; picture-in-picture'
    iframe.allowFullscreen = true
    container.append(iframe)
    player = new YT.Player(iframe, {
      events: {
        onReady: ({ target }) => {
          if (disposed) return
          seekTimer = window.setInterval(() => observeSeek(target), 250)
          events.ready({
            position: () => target.getCurrentTime(),
            duration: () => target.getDuration(),
            sync({ position, playing, seek, gesture }) {
              if (seek === 'drift' && observeSeek(target)) return
              if (gesture) {
                priming = !playing
                if (priming) target.mute()
                else target.unMute()
              } else if (playing && priming) {
                priming = false
                target.unMute()
              }
              const state = target.getPlayerState()
              if (!playing && !priming && (state === 1 || state === 3 || playTimer)) {
                reportedPlaying = false
                target.pauseVideo()
                resetPlayTimer()
              }
              // YouTube starts playing when seeking from the unstarted state, even if the room is paused.
              if (seek === 'immediate' || (seek === 'drift' && state !== 3)) {
                if (!playing && !priming && state !== 1 && state !== 2 && state !== 3) {
                  priming = true
                  target.mute()
                }
                roomSeek = { position, until: performance.now() + 5000 }
                target.seekTo(position, true)
              }
              if ((playing || gesture) && state !== 1 && (gesture || (state !== 3 && !playTimer))) {
                resetPlayTimer()
                target.playVideo()
                playTimer = window.setTimeout(() => {
                  playTimer = 0
                  const current = target.getPlayerState()
                  if (current !== 1 && current !== 3) autoplayBlocked()
                }, 5000)
              }
            },
          })
        },
        onStateChange: ({ target, data }) => {
          if (disposed) return
          if (data === 0) reportedPlaying = false
          if (data === 2 && reportedPlaying) {
            reportedPlaying = false
            events.playbackChanged(true)
          }
          if (data !== 1) return
          resetPlayTimer()
          if (priming) {
            priming = false
            reportedPlaying = false
            target.pauseVideo()
            target.unMute()
          } else if (!reportedPlaying) {
            reportedPlaying = true
            events.playbackChanged(false)
          }
          events.started()
        },
        onAutoplayBlocked: autoplayBlocked,
        onError: ({ data }) => {
          if (disposed) return
          resetPlayTimer()
          const message = data === 101 || data === 150 ? 'Автор запретил встраивание этого видео YouTube.'
            : data === 100 ? 'Видео YouTube удалено, скрыто или недоступно.'
              : data === 153 ? 'YouTube не получил данные о сайте. Проверьте настройки защиты браузера.'
                : 'YouTube не смог воспроизвести это видео.'
          events.failed(message)
        },
      },
    })
  }).catch((error: unknown) => {
    if (!disposed) events.failed(error instanceof Error ? error.message : 'Не удалось открыть плеер YouTube.')
  })

  return () => {
    disposed = true
    resetPlayTimer()
    window.clearInterval(seekTimer)
    player?.destroy()
    iframe.remove()
  }
}

export function mountVideo(video: HTMLVideoElement, type: 'mp4' | 'hls', url: string, events: MediaEvents): () => void {
  const nativeHls = type === 'hls' && Boolean(video.canPlayType('application/vnd.apple.mpegurl'))
  let disposed = false
  let ready = false
  let muteBeforePriming: boolean | undefined
  let reportedPlaying = false
  let pending = false
  let hls: Hls | undefined
  let roomSeek: number | undefined

  function seeked() {
    const position = video.currentTime
    const expected = roomSeek
    roomSeek = undefined
    if (expected === undefined || Math.abs(position - expected) > 0.15) events.seeked(position)
  }

  function finishPriming() {
    if (muteBeforePriming === undefined) return
    video.muted = muteBeforePriming
    muteBeforePriming = undefined
  }

  function started() {
    if (muteBeforePriming !== undefined) video.pause()
    finishPriming()
    events.started()
  }

  function played() {
    if (muteBeforePriming !== undefined || video.paused || reportedPlaying) return
    reportedPlaying = true
    events.playbackChanged(false)
  }

  function paused() {
    if (!reportedPlaying) return
    reportedPlaying = false
    if (!video.ended) events.playbackChanged(true)
  }

  function play() {
    if (pending || !video.paused) return
    pending = true
    void video.play().catch((error: unknown) => {
      if (disposed) return
      finishPriming()
      const name = error instanceof DOMException ? error.name : ''
      if (name === 'NotAllowedError') events.blocked()
      else if (name !== 'AbortError') events.failed('Браузер не смог воспроизвести это видео.')
    }).finally(() => { pending = false })
  }

  function metadata() {
    if (nativeHls && video.duration === Infinity) {
      events.failed('Прямые трансляции HLS не поддерживаются. Нужна запись с конечной длительностью.')
      return
    }
    if (ready) return
    ready = true
    events.ready({
      position: () => video.currentTime,
      duration: () => video.duration,
      sync({ position, playing, seek, gesture }) {
        if (playing) finishPriming()
        else if (gesture && video.paused && muteBeforePriming === undefined) {
          muteBeforePriming = video.muted
          video.muted = true
        }
        if (!playing && muteBeforePriming === undefined && !video.paused) {
          reportedPlaying = false
          video.pause()
        }
        if (seek !== 'none' && (!video.seeking || (seek === 'immediate' && !events.canSeek()))) {
          video.currentTime = position
          roomSeek = video.currentTime
        }
        if (playing || gesture) play()
      },
    })
  }

  function failed() {
    const message = video.error?.code === MediaError.MEDIA_ERR_SRC_NOT_SUPPORTED
      ? 'Браузер не поддерживает формат видео или не может открыть эту ссылку.'
      : 'Не удалось загрузить видео. Проверьте ссылку и доступ к источнику.'
    events.failed(message)
  }
  video.addEventListener('loadedmetadata', metadata)
  video.addEventListener('play', played)
  video.addEventListener('pause', paused)
  video.addEventListener('playing', started)
  video.addEventListener('seeked', seeked)
  video.addEventListener('error', failed)
  if (type === 'mp4' || nativeHls) {
    video.src = url
  } else {
    void import('hls.js').then(({ default: Hls }) => {
      if (disposed) return
      if (!Hls.isSupported()) {
        events.failed('Этот браузер не поддерживает воспроизведение HLS.')
        return
      }
      hls = new Hls()
      hls.on(Hls.Events.LEVEL_LOADED, (_, { details }) => {
        if (details.live) events.failed('Прямые трансляции HLS не поддерживаются. Нужна запись с конечной длительностью.')
      })
      hls.on(Hls.Events.ERROR, (_, { fatal, type: errorType }) => {
        if (!fatal) return
        events.failed(errorType === Hls.ErrorTypes.NETWORK_ERROR
          ? 'Не удалось загрузить HLS. Проверьте ссылку и настройки CORS источника.'
          : 'Браузер не смог воспроизвести этот поток HLS.')
      })
      hls.loadSource(url)
      hls.attachMedia(video)
    }).catch(() => {
      if (!disposed) events.failed('Не удалось запустить воспроизведение HLS.')
    })
  }

  return () => {
    disposed = true
    video.removeEventListener('loadedmetadata', metadata)
    video.removeEventListener('play', played)
    video.removeEventListener('pause', paused)
    video.removeEventListener('playing', started)
    video.removeEventListener('seeked', seeked)
    video.removeEventListener('error', failed)
    finishPriming()
    hls?.destroy()
    video.pause()
    video.removeAttribute('src')
    video.load()
  }
}
