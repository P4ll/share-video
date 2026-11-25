import { useCallback, useEffect, useRef, useState } from 'react'

export type Source = { type: 'youtube'; videoId: string } | { type: 'mp4' | 'hls'; url: string }
export type RoomState = {
  source: Source
  paused: boolean
  positionSec: number
  updatedAtMs: number
  revision: number
}
export type Command = { type: 'play' | 'pause' } | { type: 'seek'; positionSec: number }
type HostCommand = { type: 'load'; url: string }
  | { type: 'create_invitation' }
  | { type: 'set_guest_control'; guestNumber: number; canControl: boolean }
type RoomRole = 'host' | 'guest'
export type Invitation = { number: number; token: string; canControl: boolean }
type Access = { canControl: boolean; guestNumber?: number; invitations?: Invitation[] }
type RoomError = { type: 'error'; code: string }
type Message = RoomError
  | { type: 'pong'; clientNowMs: number; serverNowMs: number }
  | ({ type: 'joined'; role: RoomRole; state: RoomState; serverNowMs: number } & Access)
  | ({ type: 'access' } & Access)
  | { type: 'state'; state: RoomState; serverNowMs: number }
type Status = 'connecting' | 'connected' | 'reconnecting' | 'offline' | 'closed'

const hostKey = (roomId: string) => `share-video:host:${roomId}`
const now = () => performance.timeOrigin + performance.now()

export function joinMessage(inviteToken: string | null, readHostToken: () => string | null) {
  if (inviteToken !== null) return { type: 'join', guestToken: inviteToken }
  const hostToken = readHostToken()
  return hostToken ? { type: 'join', hostToken } : null
}

export function errorText(code: string): string {
  switch (code) {
    case 'invalid_source': return 'Эта ссылка не поддерживается. Вставьте ссылку YouTube или прямую HTTPS-ссылку на MP4 или HLS (.m3u8).'
    case 'room_not_found': return 'Комната больше недоступна. Попросите ведущего прислать новое приглашение.'
    case 'invalid_host_token': return 'Права ведущего не подтверждены. Для входа в комнату нужно приглашение.'
    case 'invalid_guest_token': return 'Для входа в комнату нужна действующая ссылка-приглашение. Попросите её у ведущего.'
    case 'forbidden': return 'У вас нет прав на это действие.'
    case 'invitation_capacity': return 'Достигнут лимит приглашений для этой комнаты.'
    case 'invitation_not_found': return 'Приглашение не найдено.'
    case 'invalid_message': return 'Сервер не принял команду.'
    case 'room_capacity': return 'Сервер занят. Попробуйте создать комнату чуть позже.'
    case 'offline': return 'Нет подключения к интернету.'
    case 'storage': return 'Браузер не разрешил сохранить доступ ведущего. Разрешите хранение данных для этого сайта и попробуйте снова.'
    default: return 'Не удалось выполнить запрос. Попробуйте ещё раз.'
  }
}

export async function createRoom(url: string): Promise<string> {
  let response: Response
  try {
    response = await fetch('/api/rooms', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ url }),
    })
  } catch {
    throw new Error(errorText(navigator.onLine ? 'connection' : 'offline'))
  }
  const data = await response.json().catch(() => null)
  if (!response.ok) throw new Error(errorText(data?.code ?? (response.status === 429 ? 'room_capacity' : 'connection')))
  if (typeof data?.roomId !== 'string' || typeof data.hostToken !== 'string') throw new Error(errorText('connection'))
  try {
    sessionStorage.setItem(hostKey(data.roomId), data.hostToken)
  } catch {
    throw new Error(errorText('storage'))
  }
  return data.roomId
}

export function useRoom(roomId: string, inviteToken: string | null) {
  const [status, setStatus] = useState<Status>(() => navigator.onLine ? 'connecting' : 'offline')
  const [state, setState] = useState<RoomState>()
  const [role, setRole] = useState<RoomRole>()
  const [access, setAccess] = useState<Access>({ canControl: false })
  const [error, setError] = useState<string>()
  const socketRef = useRef<WebSocket | null>(null)
  const offset = useRef(0)
  const serverNow = useCallback(() => now() + offset.current, [])

  useEffect(() => {
    const join = joinMessage(inviteToken, () => {
      try {
        return sessionStorage.getItem(hostKey(roomId))
      } catch {
        return null
      }
    })
    if (!join) {
      setError('invalid_guest_token')
      setStatus('closed')
      return
    }
    const url = new URL(`/api/rooms/${encodeURIComponent(roomId)}/ws`, location.origin)
    url.protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
    let terminal = false
    let retry = 500
    let revision = -1
    let retryTimer = 0
    let handshake = 0
    let heartbeat = 0

    function disconnect() {
      window.clearTimeout(handshake)
      window.clearInterval(heartbeat)
      const socket = socketRef.current
      socketRef.current = null
      socket?.close()
    }

    function connect() {
      if (terminal || socketRef.current) return
      if (!navigator.onLine) {
        setStatus('offline')
        return
      }
      setStatus(revision < 0 && retry === 500 ? 'connecting' : 'reconnecting')
      const socket = new WebSocket(url.href)
      socketRef.current = socket
      let joined = false
      let lastMessage = 0
      let sentPing = 0
      let bestRoundTrip = Infinity
      handshake = window.setTimeout(() => socket.close(), 10_000)

      function ping() {
        sentPing = Math.round(now())
        socket.send(JSON.stringify({ type: 'ping', clientNowMs: sentPing }))
      }

      socket.onopen = () => {
        if (socketRef.current === socket) socket.send(JSON.stringify(join))
      }
      socket.onmessage = (event) => {
        if (socketRef.current !== socket || typeof event.data !== 'string') return
        let message: Message
        try {
          message = JSON.parse(event.data)
        } catch {
          return
        }
        if (!message || typeof message !== 'object') return
        lastMessage = now()
        switch (message.type) {
          case 'joined':
            joined = true
            retry = 500
            offset.current = message.serverNowMs - lastMessage
            revision = message.state.revision
            setRole(message.role)
            setAccess(message)
            setState(message.state)
            setError(undefined)
            setStatus('connected')
            window.clearTimeout(handshake)
            ping()
            heartbeat = window.setInterval(() => {
              if (socket.readyState !== WebSocket.OPEN) return
              if (now() - lastMessage > 20_000) socket.close()
              else ping()
            }, 5000)
            break
          case 'access':
            if (joined) setAccess(message)
            break
          case 'state':
            if (joined && message.state.revision > revision) {
              revision = message.state.revision
              setState(message.state)
            }
            break
          case 'pong': {
            if (!joined || message.clientNowMs !== sentPing) break
            const roundTrip = lastMessage - sentPing
            if (roundTrip <= bestRoundTrip + 25) {
              bestRoundTrip = Math.min(bestRoundTrip, roundTrip)
              offset.current = message.serverNowMs - (sentPing + roundTrip / 2)
            }
            break
          }
          case 'error':
            setError(message.code)
            if (!joined || message.code === 'room_not_found') {
              terminal = true
              disconnect()
              setStatus('closed')
            }
        }
      }
      socket.onclose = () => {
        if (socketRef.current !== socket) return
        disconnect()
        if (!navigator.onLine) {
          setStatus('offline')
          return
        }
        setStatus('reconnecting')
        retryTimer = window.setTimeout(connect, retry)
        retry = Math.min(8000, retry * 2)
      }
    }

    function offline() {
      if (terminal) return
      window.clearTimeout(retryTimer)
      disconnect()
      setStatus('offline')
    }
    function online() {
      window.clearTimeout(retryTimer)
      connect()
    }
    window.addEventListener('offline', offline)
    window.addEventListener('online', online)
    connect()
    return () => {
      terminal = true
      window.removeEventListener('offline', offline)
      window.removeEventListener('online', online)
      window.clearTimeout(retryTimer)
      disconnect()
    }
  }, [roomId, inviteToken])

  function send(command: Command | HostCommand): boolean {
    const socket = socketRef.current
    if (status !== 'connected' || !socket || socket.readyState !== WebSocket.OPEN) return false
    setError(undefined)
    socket.send(JSON.stringify(command))
    return true
  }

  const { canControl, invitations, guestNumber } = access
  return { status, state, role, canControl, invitations, guestNumber, serverNow, error, send }
}
