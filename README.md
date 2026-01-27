# Edge Backend
## Directory Structure

```
back/
├── src/                    # Application source code
│   ├── database/           # Data layer (Redis + models)
│   │   └── stream/         # Stream/game data models
│   └── server/             # HTTP server implementation
│       ├── api/            # Route handlers (controllers)
│       ├── dtos/           # Data transfer objects
│       ├── extractors/     # Request extractors (auth, validation)
│       └── services/       # Logic services
├── tests/                  # Integration tests
├── keys/                   # Cryptographic key storage
├── Cargo.toml              # Rust dependencies
├── Dockerfile              # Container build configuration
├── docker-compose.test.yml # Test environment setup
└── fly.toml                # Fly deployment config
```

## Source Modules

### `src/config.rs`
Application configuration loaded from environment variables:
- `CARGO_ENV` - Environment (development/production)
- `PORT` - Server port (default: 5000)
- `REDIS_URL` - Redis connection URL (required)
- `ACCESS_TOKEN_SECRET` - Secret for HMAC signatures
- `CORS_ORIGIN` - Allowed CORS origins (comma-separated)
- `PREVIEW_CORS_ORIGIN` - Preview environment CORS origins
- `SENTRY_DSN` - Optional Sentry error tracking

### `src/logger.rs`
Logging with tracing subscriber configuration, Sentry integration, and custom panic hooks for detailed error reporting.

### `src/main.rs`
Application entry point - loads environment, initializes services, connects to Redis, and starts the server.

### `src/database/`

| Module | Description |
|--------|-------------|
| `redis_connection.rs` | Redis connection pooling, health checks, and data operations |
| `stream/model.rs` | `Stream` and `Game` struct definitions |
| `stream/repository.rs` | Data access methods for streams and games |

### `src/server/`

| Module | Description |
|--------|-------------|
| `mod.rs` | Server initialization, routing, middleware (CORS, rate limiting, timeouts), metrics |
| `error.rs` | Error types mapping to HTTP status codes (401, 403, 404, 429, 500, etc.) |

### `src/server/api/`

| Controller | Description |
|------------|-------------|
| `health_controller.rs` | Health check endpoints with service status |
| `stream_controller.rs` | Stream/game data endpoints |
| `proxy_controller.rs` | HTTP proxy for streaming content |

### `src/server/services/`

| Service | Description |
|---------|-------------|
| `edge_services.rs` | Central service container and orchestration |
| `stream_services.rs` | Stream data fetching and caching |
| `ppvsu_services.rs` | PPVSU game fetching, link decoding, cache management |
| `rate_limit_services.rs` | Per-client rate limiting via Redis |
| `cookie_services.rs` | Domain-specific cookie storage for proxy requests |

### `src/server/extractors/`

| Extractor | Description |
|-----------|-------------|
| `edge_authentication_extractor.rs` | Client ID generation (IP + User-Agent hash), signature verification |

### `src/server/dtos/`

| DTO | Description |
|-----|-------------|
| `health_dto.rs` | Health status response structures |
| `stream_dto.rs` | Game, category, and stream response structures |

---

## API Endpoints

### Health

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| GET | `/` | None | Basic health check |
| GET | `/api/v1/health` | None | Detailed health status with service checks |
| GET | `/metrics` | None | Prometheus metrics |

#### `GET /api/v1/health`
Returns system health including Redis status and response time.

```json
{
  "status": "healthy",
  "timestamp": "2024-01-27T12:00:00Z",
  "uptime_seconds": 3600,
  "version": "0.0.1",
  "environment": "production",
  "services": {
    "database": { "status": "healthy", "response_time_ms": 0 },
    "redis": { "status": "healthy", "response_time_ms": 1.5 }
  }
}
```

---

### Streams

All stream endpoints require Edge authentication (client ID derived from IP + User-Agent). (or not because it'll let you through anyway for this case, you can change this in the edge_authentication file at the bottom to instead return Err)

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/streams/` | List all games grouped by category |
| GET | `/api/v1/streams/{provider}` | Get stream data for specific provider |
| GET | `/api/v1/streams/ppvsu/{id}` | Get PPVSU game by ID |
| GET | `/api/v1/streams/ppvsu/{id}/decode` | Decode encrypted video link |
| GET | `/api/v1/streams/ppvsu/{id}/signed-url` | Generate signed proxy URL (12hr expiry) |
| DELETE | `/api/v1/streams/ppvsu/cache` | Clear PPVSU Redis cache |

#### `GET /api/v1/streams/`
Returns all games organized by category.

```json
{
  "categories": [
    {
      "category": "Sports",
      "games": [
        {
          "id": 1,
          "name": "Game Name",
          "poster": "https://...",
          "start_time": 1704067200,
          "end_time": 1704070800,
          "cache_time": 1704067200,
          "video_link": "encoded_link",
          "category": "Sports"
        }
      ]
    }
  ]
}
```

#### `GET /api/v1/streams/ppvsu/{id}/signed-url`
Generates a signed URL for proxy access.

```json
{
  "signed_url": "/api/v1/proxy?url=..&schema=sports&sig=..&exp=..&client=..",
  "expires_at": 1704071400
}
```

---

### Proxy

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/proxy` | Proxy streaming content with signature verification |
| OPTIONS | `/api/v1/proxy` | CORS preflight |

#### `GET /api/v1/proxy`
Proxies HTTP requests to external streaming servers.

**Query Parameters:**
| Parameter | Required | Description |
|-----------|----------|-------------|
| `url` | Yes | Target URL (base64-encoded or plain HTTP/HTTPS) |
| `schema` | No | Request schema (e.g., "sports") |
| `sig` | No | HMAC signature for verification |
| `exp` | No | Expiration timestamp |
| `client` | No | Client identifier for signature verification |

**Response Behavior:**
- **M3U8 playlists**: Rewrites URLs, applies compression, `Cache-Control: no-cache`

---

## Development

### Prerequisites
- Rust (latest stable)
- Redis

### Environment Setup
Copy `.env.example` to `.env` and configure:

```bash
CARGO_ENV=development
PORT=5000
REDIS_URL=redis://localhost:6379
ACCESS_TOKEN_SECRET=your_secret_here
CORS_ORIGIN=http://localhost:3000
```

### Start Redis
```bash
docker-compose -f docker-compose.test.yml up redis -d
```

### Run the Server
```bash
cargo run
```

### Run Tests
```bash
REDIS_URL=redis://localhost:6379 cargo test
```

---
