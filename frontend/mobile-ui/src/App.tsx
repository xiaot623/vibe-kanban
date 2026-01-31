import { useEffect, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'

type DiscoveredServer = {
  ip: string
  port: number
  hostname: string
  requires_auth: boolean
}

type ServerEntry = DiscoveredServer & {
  status: 'validated' | 'needs_auth'
}

type Step = 'url' | 'password'

const STORAGE_URL = 'vibe_kanban_server_url'
const STORAGE_PASSWORD = 'vibe_kanban_server_password'
const BASIC_AUTH_USER = 'vibe'

const normalizeUrl = (value: string) => {
  let url = value.trim()
  if (!url) return ''
  if (!url.startsWith('http://') && !url.startsWith('https://')) {
    url = `http://${url}`
  }
  return url
}

const buildAuthUrl = (url: string, password?: string) => {
  if (!password) return url
  try {
    const parsed = new URL(url)
    parsed.username = BASIC_AUTH_USER
    parsed.password = password
    return parsed.toString()
  } catch {
    return url
  }
}

const buildAuthHeader = (password: string) => {
  return `Basic ${btoa(`${BASIC_AUTH_USER}:${password}`)}`
}

const serverKey = (server: DiscoveredServer) => `${server.ip}:${server.port}`

const fetchHealth = async (server: DiscoveredServer, password?: string) => {
  const url = `http://${server.ip}:${server.port}/api/health`
  const headers: HeadersInit = {}
  if (password) {
    headers.Authorization = buildAuthHeader(password)
  }

  try {
    const response = await fetch(url, { headers })
    console.log('[fetchHealth] response status:', response.status)
    if (!response.ok) {
      console.log('[fetchHealth] response not ok:', response.status, response.statusText)
      return false
    }

    const payload = await response.json().catch((e) => {
      console.log('[fetchHealth] json parse error:', e)
      return null
    })
    console.log('[fetchHealth] payload:', payload)
    return payload?.data?.service === 'vibe-kanban'
  } catch (e) {
    console.error('[fetchHealth] fetch error:', e)
    throw e
  }
}

function App() {
  const [step, setStep] = useState<Step>('url')
  const [serverUrl, setServerUrl] = useState('')
  const [password, setPassword] = useState('')
  const [showPassword, setShowPassword] = useState(false)
  const [manualError, setManualError] = useState('')
  const [scanError, setScanError] = useState('')
  const [isConnecting, setIsConnecting] = useState(false)
  const [isDark, setIsDark] = useState(false)
  const [isScanning, setIsScanning] = useState(false)
  const [servers, setServers] = useState<ServerEntry[]>([])
  const [serverPasswords, setServerPasswords] = useState<Record<string, string>>({})
  const [serverErrors, setServerErrors] = useState<Record<string, string>>({})
  const [connectingKey, setConnectingKey] = useState<string | null>(null)

  useEffect(() => {
    const mediaQuery = window.matchMedia('(prefers-color-scheme: dark)')
    setIsDark(mediaQuery.matches)

    const handleChange = (e: MediaQueryListEvent) => {
      setIsDark(e.matches)
    }

    mediaQuery.addEventListener('change', handleChange)
    return () => mediaQuery.removeEventListener('change', handleChange)
  }, [])

  const runDiscovery = async () => {
    setIsScanning(true)
    setScanError('')
    setServers([])
    setServerErrors({})
    setServerPasswords({})

    try {
      const discovered = await invoke<DiscoveredServer[]>('discover_servers')
      const validated = await Promise.all(
        discovered.map(async (server) => {
          if (server.requires_auth) {
            return { ...server, status: 'needs_auth' } as ServerEntry
          }
          try {
            const isValid = await fetchHealth(server)
            return isValid ? ({ ...server, status: 'validated' } as ServerEntry) : null
          } catch {
            return null
          }
        })
      )

      const filtered = validated.filter(Boolean) as ServerEntry[]
      setServers(filtered)
      if (filtered.length === 0) {
        setScanError('No servers found on the local network.')
      }
    } catch {
      setScanError('Unable to scan the local network.')
    } finally {
      setIsScanning(false)
    }
  }

  useEffect(() => {
    const savedUrl = localStorage.getItem(STORAGE_URL)
    const savedPassword = localStorage.getItem(STORAGE_PASSWORD)
    if (savedUrl) {
      setServerUrl(savedUrl)
      setIsConnecting(true)
      setTimeout(() => {
        window.location.href = buildAuthUrl(savedUrl, savedPassword ?? undefined)
      }, 500)
      return
    }

    void runDiscovery()
  }, [])

  const connectToUrl = (url: string, passwordValue?: string) => {
    localStorage.setItem(STORAGE_URL, url)
    if (passwordValue) {
      localStorage.setItem(STORAGE_PASSWORD, passwordValue)
    } else {
      localStorage.removeItem(STORAGE_PASSWORD)
    }
    setIsConnecting(true)
    window.location.href = buildAuthUrl(url, passwordValue)
  }

  const handleNextStep = () => {
    const url = normalizeUrl(serverUrl)
    if (!url) {
      setManualError('Please enter a URL')
      return
    }

    try {
      new URL(url)
      setServerUrl(url)
      setManualError('')
      setStep('password')
    } catch {
      setManualError('Invalid URL')
    }
  }

  const handleConnect = () => {
    if (!serverUrl) {
      setManualError('Please enter a URL')
      setStep('url')
      return
    }

    try {
      new URL(serverUrl)
      const trimmedPassword = password.trim()
      setManualError('')
      connectToUrl(serverUrl, trimmedPassword || undefined)
    } catch {
      setManualError('Invalid URL')
      setStep('url')
    }
  }

  const handleBack = () => {
    setStep('url')
    setPassword('')
    setManualError('')
  }

  const handleUrlKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') {
      handleNextStep()
    }
  }

  const handlePasswordKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') {
      handleConnect()
    }
  }

  const handleServerConnect = async (server: ServerEntry) => {
    const key = serverKey(server)
    setConnectingKey(key)
    setServerErrors((prev) => ({ ...prev, [key]: '' }))

    const localPassword = serverPasswords[key]?.trim() ?? ''
    if (server.requires_auth && !localPassword) {
      setServerErrors((prev) => ({
        ...prev,
        [key]: 'Enter the local network password to continue.',
      }))
      setConnectingKey(null)
      return
    }

    try {
      const isValid = await fetchHealth(
        server,
        server.requires_auth ? localPassword : undefined
      )
      if (!isValid) {
        setServerErrors((prev) => ({
          ...prev,
          [key]: 'Unable to validate this server. Check the password and try again.',
        }))
        setConnectingKey(null)
        return
      }
    } catch {
      setServerErrors((prev) => ({
        ...prev,
        [key]: 'Unable to reach this server. Check your network and try again.',
      }))
      setConnectingKey(null)
      return
    }

    connectToUrl(`http://${server.ip}:${server.port}`, localPassword || undefined)
  }

  return (
    <div className={`new-design ${isDark ? 'dark' : ''}`}>
      <div className="min-h-screen bg-primary flex flex-col items-center justify-center p-base">
        <div className="w-full max-w-[440px] bg-secondary rounded-lg p-double shadow-lg">
          <h1 className="text-xl font-semibold text-high text-center mb-base">
            Connect to Vibe Kanban
          </h1>
          <p className="text-sm text-low text-center mb-double">
            We can scan your local network or you can enter a server address manually.
          </p>

          <div className="mb-double">
            <div className="flex items-center justify-between mb-base">
              <h2 className="text-base font-semibold text-high">Nearby servers</h2>
              <button
                onClick={() => void runDiscovery()}
                disabled={isScanning}
                className="text-sm text-brand-secondary hover:text-brand disabled:opacity-50"
              >
                Scan again
              </button>
            </div>

            {isScanning && (
              <div className="flex items-center gap-base text-low mb-base">
                <div className="h-4 w-4 rounded-full border-2 border-brand border-t-transparent animate-spin" />
                <span className="text-sm">Scanning local network...</span>
              </div>
            )}

            {!isScanning && scanError && (
              <p className="text-sm text-low mb-base">{scanError}</p>
            )}

            <div className="space-y-base">
              {servers.map((server) => {
                const key = serverKey(server)
                const isConnectingServer = connectingKey === key
                return (
                  <div
                    key={key}
                    className="border border-border rounded-lg p-base bg-panel"
                  >
                    <div className="flex items-start justify-between gap-base">
                      <div>
                        <p className="text-base font-semibold text-high">
                          {server.hostname || 'Vibe Kanban Server'}
                        </p>
                        <p className="text-sm text-low">{key}</p>
                      </div>
                      {server.requires_auth && (
                        <span className="text-xs text-brand-secondary">Password required</span>
                      )}
                    </div>

                    {server.requires_auth && (
                      <div className="mt-base">
                        <input
                          type="password"
                          value={serverPasswords[key] ?? ''}
                          onChange={(e) => {
                            setServerPasswords((prev) => ({
                              ...prev,
                              [key]: e.target.value,
                            }))
                            setServerErrors((prev) => ({ ...prev, [key]: '' }))
                          }}
                          placeholder="Local network password"
                          className="w-full px-base py-[10px] bg-primary rounded border text-base text-normal placeholder:text-low focus:outline-none focus:ring-1 focus:ring-brand"
                          disabled={isConnecting || isConnectingServer}
                        />
                      </div>
                    )}

                    <button
                      onClick={() => void handleServerConnect(server)}
                      disabled={isConnecting || isConnectingServer}
                      className="mt-base w-full py-[10px] bg-brand text-on-brand font-semibold rounded text-sm hover:bg-brand-hover focus:outline-none focus:ring-2 focus:ring-brand disabled:opacity-50 disabled:cursor-not-allowed"
                    >
                      {isConnectingServer ? 'Connecting...' : 'Connect'}
                    </button>

                    {serverErrors[key] && (
                      <p className="text-sm text-error mt-base">{serverErrors[key]}</p>
                    )}

                    {server.status === 'needs_auth' && !serverErrors[key] && (
                      <p className="text-xs text-low mt-base">
                        Enter the password to validate this server.
                      </p>
                    )}
                  </div>
                )
              })}
            </div>
          </div>

          <div className="border-t border-border pt-double">
            <h2 className="text-base font-semibold text-high mb-base">Or enter manually</h2>

            {step === 'url' ? (
              <>
                <input
                  type="url"
                  value={serverUrl}
                  onChange={(e) => {
                    setServerUrl(e.target.value)
                    setManualError('')
                  }}
                  onKeyDown={handleUrlKeyDown}
                  placeholder="http://192.168.1.x:3000"
                  className="w-full px-base py-[10px] bg-primary rounded border text-base text-normal placeholder:text-low focus:outline-none focus:ring-1 focus:ring-brand mb-base"
                  disabled={isConnecting}
                />

                <button
                  onClick={handleNextStep}
                  disabled={isConnecting}
                  className="w-full py-[12px] bg-brand text-on-brand font-semibold rounded text-base hover:bg-brand-hover focus:outline-none focus:ring-2 focus:ring-brand disabled:opacity-50 disabled:cursor-not-allowed"
                >
                  {isConnecting ? 'Connecting...' : 'Next'}
                </button>
              </>
            ) : (
              <>
                <p className="text-sm text-low text-center mb-half">{serverUrl}</p>
                <p className="text-sm text-low text-center mb-double">
                  Enter the password if required, or skip to connect without auth.
                </p>

                <div className="relative mb-base">
                  <input
                    type={showPassword ? 'text' : 'password'}
                    value={password}
                    onChange={(e) => {
                      setPassword(e.target.value)
                      setManualError('')
                    }}
                    onKeyDown={handlePasswordKeyDown}
                    placeholder="Password (optional)"
                    className="w-full px-base py-[10px] pr-[40px] bg-primary rounded border text-base text-normal placeholder:text-low focus:outline-none focus:ring-1 focus:ring-brand"
                    disabled={isConnecting}
                    autoFocus
                  />
                  <button
                    type="button"
                    onClick={() => setShowPassword(!showPassword)}
                    className="absolute right-[10px] top-1/2 -translate-y-1/2 text-low hover:text-normal focus:outline-none"
                    tabIndex={-1}
                  >
                    {showPassword ? (
                      <svg
                        xmlns="http://www.w3.org/2000/svg"
                        fill="none"
                        viewBox="0 0 24 24"
                        strokeWidth={1.5}
                        stroke="currentColor"
                        className="w-5 h-5"
                      >
                        <path
                          strokeLinecap="round"
                          strokeLinejoin="round"
                          d="M3.98 8.223A10.477 10.477 0 001.934 12C3.226 16.338 7.244 19.5 12 19.5c.993 0 1.953-.138 2.863-.395M6.228 6.228A10.45 10.45 0 0112 4.5c4.756 0 8.773 3.162 10.065 7.498a10.523 10.523 0 01-4.293 5.774M6.228 6.228L3 3m3.228 3.228l3.65 3.65m7.894 7.894L21 21m-3.228-3.228l-3.65-3.65m0 0a3 3 0 10-4.243-4.243m4.242 4.242L9.88 9.88"
                        />
                      </svg>
                    ) : (
                      <svg
                        xmlns="http://www.w3.org/2000/svg"
                        fill="none"
                        viewBox="0 0 24 24"
                        strokeWidth={1.5}
                        stroke="currentColor"
                        className="w-5 h-5"
                      >
                        <path
                          strokeLinecap="round"
                          strokeLinejoin="round"
                          d="M2.036 12.322a1.012 1.012 0 010-.639C3.423 7.51 7.36 4.5 12 4.5c4.638 0 8.573 3.007 9.963 7.178.07.207.07.431 0 .639C20.577 16.49 16.64 19.5 12 19.5c-4.638 0-8.573-3.007-9.963-7.178z"
                        />
                        <path
                          strokeLinecap="round"
                          strokeLinejoin="round"
                          d="M15 12a3 3 0 11-6 0 3 3 0 016 0z"
                        />
                      </svg>
                    )}
                  </button>
                </div>

                <div className="flex gap-base">
                  <button
                    onClick={handleBack}
                    disabled={isConnecting}
                    className="flex-1 py-[12px] bg-primary border text-normal font-semibold rounded text-base hover:bg-secondary focus:outline-none focus:ring-2 focus:ring-brand disabled:opacity-50 disabled:cursor-not-allowed"
                  >
                    Back
                  </button>
                  <button
                    onClick={handleConnect}
                    disabled={isConnecting}
                    className="flex-1 py-[12px] bg-brand text-on-brand font-semibold rounded text-base hover:bg-brand-hover focus:outline-none focus:ring-2 focus:ring-brand disabled:opacity-50 disabled:cursor-not-allowed"
                  >
                    {isConnecting ? 'Connecting...' : 'Connect'}
                  </button>
                </div>
              </>
            )}

            {manualError && (
              <p className="text-sm text-error mt-base">{manualError}</p>
            )}
          </div>
        </div>
      </div>
    </div>
  )
}

export default App
