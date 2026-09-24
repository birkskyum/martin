use std::collections::BTreeMap;
use std::sync::Arc;

use actix_web::web::{self, ServiceConfig};
use compact_str::CompactString;
use martin_core::tiles::Tile;
use martin_tile_utils::{Format, TileCoord, tile_index};
use mlt_core::geo_types::Geometry;
use mlt_core::mvt::mvt_to_feature_collection;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData as McpError, ServerHandler, schemars, tool, tool_handler, tool_router};
use rmcp_actix_web::transport::{LocalSessionManager, StreamableHttpService};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::file::ServerState;
use crate::config::file::srv::SrvConfig;
use crate::srv::tiles::content::decode;
use crate::srv::{Catalog, merge_tilejson};
use crate::tile_source_manager::TileSourceManager;

/// How many features of each source layer `get_features` shows, unless the caller asks for another number.
const DEFAULT_FEATURE_LIMIT: usize = 5;
/// How many distinct values of each field `get_features` lists.
const MAX_FIELD_VALUES: usize = 8;

const INSTRUCTIONS: &str = "Martin serves map tiles, sprites, fonts and styles. \
    Call list_sources to see what it serves, describe_source for the source layers and fields of a tile source, \
    and get_features for the features and field values in one tile, before writing MapLibre style layers against them.";

/// Read-only tools that let AI agents see what this Martin server serves, over MCP.
#[derive(Clone)]
pub struct MartinTools {
    catalog: Catalog,
    tiles: TileSourceManager,
    /// Where Martin's routes are mounted, prepended to the paths the tools report.
    prefix: String,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SourceRequest {
    /// A tile source id from `list_sources`, or several joined by commas to combine them.
    source: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct FeaturesRequest {
    /// A tile source id from `list_sources`. Unlike tile URLs, one source at a time.
    source: String,
    /// Longitude of a place inside the tile to read.
    longitude: f64,
    /// Latitude of a place inside the tile to read.
    latitude: f64,
    /// Zoom level of the tile to read.
    zoom: u8,
    /// Only show this source layer.
    layer: Option<String>,
    /// How many features of each source layer to show. Defaults to 5.
    limit: Option<usize>,
}

/// What one source layer of a tile holds.
#[derive(Default, Serialize)]
struct LayerSummary {
    features: usize,
    geometry_types: BTreeMap<String, usize>,
    /// Up to [`MAX_FIELD_VALUES`] distinct values of each field.
    fields: BTreeMap<String, Vec<Value>>,
    examples: Vec<BTreeMap<String, Value>>,
}

#[tool_router]
impl MartinTools {
    /// Returns the tools when the configuration turns the MCP endpoint on.
    pub fn when_enabled(
        config: &SrvConfig,
        catalog: &Catalog,
        state: &ServerState,
    ) -> Option<Self> {
        if !config
            .endpoints
            .as_ref()
            .is_some_and(|endpoints| endpoints.mcp.unwrap_or(false))
        {
            return None;
        }
        Some(Self {
            catalog: catalog.clone(),
            tiles: state.tile_manager.clone(),
            prefix: config.public_path_prefix().unwrap_or_default().to_owned(),
            tool_router: Self::tool_router(),
        })
    }

    #[tool(
        description = "Lists the tile sources, sprites, fonts and styles this Martin server serves, \
        with the paths to use for them in a MapLibre style."
    )]
    async fn list_sources(&self) -> Result<CallToolResult, McpError> {
        let mut catalog = self.catalog.clone();
        catalog.tiles = self.tiles.tile_sources().get_catalog();
        let json = serde_json::to_string_pretty(&catalog).map_err(internal_error)?;
        let prefix = &self.prefix;
        let paths = format!(
            "Paths on this server: TileJSON {prefix}/<source>, tiles {prefix}/<source>/{{z}}/{{x}}/{{y}}, \
            sprites {prefix}/sprite/<id>, glyphs {prefix}/font/{{fontstack}}/{{range}}, styles {prefix}/style/<id>."
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "{paths}\n\n{json}"
        ))]))
    }

    #[tool(
        description = "Returns the TileJSON of a tile source: its zoom range, bounds, \
        and for vector tiles, the source layers with their fields and field types."
    )]
    async fn describe_source(
        &self,
        Parameters(SourceRequest { source }): Parameters<SourceRequest>,
    ) -> Result<CallToolResult, McpError> {
        let resolved = match self.tiles.tile_sources().get_sources(&source, None) {
            Ok(resolved) => resolved,
            Err(error) => return Ok(tool_error(error.to_string())),
        };
        let tiles_url = format!("{}/{source}/{{z}}/{{x}}/{{y}}", self.prefix);
        let tilejson = merge_tilejson(&resolved.sources, tiles_url);
        let json = serde_json::to_string_pretty(&tilejson).map_err(internal_error)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    #[tool(
        description = "Reads the vector tile of a source that covers a place at a zoom level, and lists for each \
        source layer how many features it has, their geometry types, up to 8 values of each field, and a few example \
        features. Use it to learn the real field values, like the classes of roads, before writing filters."
    )]
    async fn get_features(
        &self,
        Parameters(request): Parameters<FeaturesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (x, y) = tile_index(request.longitude, request.latitude, request.zoom);
        let Some(coord) = TileCoord::new_checked(request.zoom, x, y) else {
            return Ok(tool_error(format!(
                "Zoom {} is out of range.",
                request.zoom
            )));
        };
        let (source, _) = match self.tiles.tile_sources().get_source(&request.source) {
            Ok(source) => source,
            Err(error) => return Ok(tool_error(error.to_string())),
        };
        let data = match source.get_tile(coord, None).await {
            Ok(data) => data,
            Err(error) => return Ok(tool_error(error.to_string())),
        };
        if data.is_empty() {
            return Ok(tool_error(format!(
                "The tile {coord} of {} is empty.",
                request.source
            )));
        }
        let info = source.get_tile_info();
        if info.format != Format::Mvt {
            return Ok(tool_error(format!(
                "get_features reads Mapbox Vector Tiles, and {} serves {}.",
                request.source, info.format
            )));
        }
        let tile = Tile::new_with_etag(data, info, CompactString::default());
        let tile = decode(tile).map_err(internal_error)?;
        let features = match mvt_to_feature_collection(&tile.data) {
            Ok(collection) => collection.features,
            Err(error) => {
                return Ok(tool_error(format!(
                    "The tile could not be decoded: {error}"
                )));
            }
        };

        let limit = request.limit.unwrap_or(DEFAULT_FEATURE_LIMIT);
        let layers = summarize_layers(features, request.layer.as_deref(), limit);
        let tile = format!("{}/{}/{}", coord.z(), coord.x(), coord.y());
        let summary = serde_json::json!({"tile": tile, "layers": layers});
        let json = serde_json::to_string_pretty(&summary).map_err(internal_error)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }
}

#[tool_handler(router = self.tool_router)]
#[expect(
    clippy::unused_async_trait_impl,
    reason = "the methods tool_handler generates are async without awaiting"
)]
impl ServerHandler for MartinTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("martin", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Mounts the MCP endpoint at `/mcp`, after the route prefix when there is one. Like rmcp's own default,
/// it only answers requests whose `Host` header is a loopback address, which keeps web pages from reaching
/// a local server by DNS rebinding.
pub fn register(cfg: &mut ServiceConfig, tools: Option<&MartinTools>, route_prefix: Option<&str>) {
    let Some(tools) = tools.cloned() else {
        return;
    };
    let service = StreamableHttpService::builder()
        .service_factory(Arc::new(move || Ok(tools.clone())))
        .session_manager(Arc::new(LocalSessionManager::default()))
        .stateful_mode(false)
        .json_response(true)
        .build();
    let path = format!("{}/mcp", route_prefix.unwrap_or_default());
    cfg.service(web::scope(&path).service(service.scope()));
}

/// Groups the features of a tile by their source layer, and summarizes each layer.
fn summarize_layers(
    features: Vec<mlt_core::geojson::Feature>,
    only_layer: Option<&str>,
    limit: usize,
) -> BTreeMap<String, LayerSummary> {
    let mut layers: BTreeMap<String, LayerSummary> = BTreeMap::new();
    for feature in features {
        let geometry_type = geometry_type(&feature.geometry);
        let mut properties = feature.properties;
        let Some(Value::String(layer)) = properties.remove("_layer") else {
            continue;
        };
        properties.remove("_extent");
        if only_layer.is_some_and(|only| only != layer) {
            continue;
        }

        let summary = layers.entry(layer).or_default();
        summary.features += 1;
        *summary
            .geometry_types
            .entry(geometry_type.to_owned())
            .or_default() += 1;
        for (field, value) in &properties {
            let values = summary.fields.entry(field.clone()).or_default();
            if values.len() < MAX_FIELD_VALUES && !values.contains(value) {
                values.push(value.clone());
            }
        }
        if summary.examples.len() < limit {
            summary.examples.push(properties);
        }
    }
    layers
}

/// Returns the `GeoJSON` type of a geometry, like `LineString`.
fn geometry_type(geometry: &Geometry<i32>) -> &'static str {
    match geometry {
        Geometry::Point(_) => "Point",
        Geometry::Line(_) | Geometry::LineString(_) => "LineString",
        Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => "Polygon",
        Geometry::MultiPoint(_) => "MultiPoint",
        Geometry::MultiLineString(_) => "MultiLineString",
        Geometry::MultiPolygon(_) => "MultiPolygon",
        Geometry::GeometryCollection(_) => "GeometryCollection",
    }
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

fn internal_error(error: impl std::fmt::Display) -> McpError {
    McpError::internal_error(error.to_string(), None)
}
