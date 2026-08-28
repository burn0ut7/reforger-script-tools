#ifdef WORKBENCH
class RST_WorkbenchSampleTerrainRequest : JsonApiStruct
{
	float centerX;
	float centerZ;
	float halfExtentMeters;
	float spacingMeters;
	bool includeWater;

	void RST_WorkbenchSampleTerrainRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchSampleTerrainResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	float centerX;
	float centerZ;
	float halfExtentMeters;
	float requestedSpacingMeters;
	float effectiveSpacingMeters;
	bool spacingClamped;
	float gridOriginX;
	float gridOriginZ;
	int gridWidth;
	int gridHeight;
	string heights;
	string waterTypes;
	string waterSurfaceHeights;
	string waterDepthsAboveTerrain;
	float boundsMinX;
	float boundsMinY;
	float boundsMinZ;
	float boundsMaxX;
	float boundsMaxY;
	float boundsMaxZ;
	int heightmapResolutionX;
	int heightmapResolutionZ;
	float nativeSpacingMeters;
	int tileCountX;
	int tileCountZ;

	void RST_WorkbenchSampleTerrainResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchSampleTerrain : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchSampleTerrainRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchSampleTerrainRequest typedRequest = RST_WorkbenchSampleTerrainRequest.Cast(request);
		RST_WorkbenchSampleTerrainResponse response = new RST_WorkbenchSampleTerrainResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.centerX = typedRequest.centerX;
		response.centerZ = typedRequest.centerZ;
		response.halfExtentMeters = typedRequest.halfExtentMeters;
		response.requestedSpacingMeters = typedRequest.spacingMeters;
		if (typedRequest.halfExtentMeters < 0.01 || typedRequest.halfExtentMeters > 500 || typedRequest.spacingMeters < 0 || typedRequest.spacingMeters > 500)
		{
			response.status = "invalid-query";
			return response;
		}
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!api)
		{
			response.status = "world-editor-api-unavailable";
			return response;
		}
		BaseWorld world = api.GetWorld();
		if (typedRequest.includeWater && !world)
		{
			response.status = "water-world-unavailable";
			return response;
		}
		vector boundsMin, boundsMax;
		if (!editor.GetTerrainBounds(boundsMin, boundsMax))
		{
			response.status = "terrain-unavailable";
			return response;
		}
		response.boundsMinX = boundsMin[0];
		response.boundsMinY = boundsMin[1];
		response.boundsMinZ = boundsMin[2];
		response.boundsMaxX = boundsMax[0];
		response.boundsMaxY = boundsMax[1];
		response.boundsMaxZ = boundsMax[2];
		response.heightmapResolutionX = api.GetTerrainResolutionX();
		response.heightmapResolutionZ = api.GetTerrainResolutionY();
		response.nativeSpacingMeters = api.GetTerrainUnitScale();
		response.tileCountX = api.GetTerrainTilesX();
		response.tileCountZ = api.GetTerrainTilesY();
		if (response.heightmapResolutionX < 1 || response.heightmapResolutionZ < 1 || response.nativeSpacingMeters <= 0 || response.tileCountX < 1 || response.tileCountZ < 1)
		{
			response.status = "terrain-metadata-unavailable";
			return response;
		}
		response.effectiveSpacingMeters = response.nativeSpacingMeters;
		if (typedRequest.spacingMeters > 0)
			response.effectiveSpacingMeters = Math.Ceil(typedRequest.spacingMeters / response.nativeSpacingMeters) * response.nativeSpacingMeters;
		response.spacingClamped = typedRequest.spacingMeters > 0 && typedRequest.spacingMeters != response.effectiveSpacingMeters;
		int firstX = Math.Ceil((typedRequest.centerX - typedRequest.halfExtentMeters - boundsMin[0]) / response.effectiveSpacingMeters);
		int lastX = Math.Floor((typedRequest.centerX + typedRequest.halfExtentMeters - boundsMin[0]) / response.effectiveSpacingMeters);
		int firstZ = Math.Ceil((typedRequest.centerZ - typedRequest.halfExtentMeters - boundsMin[2]) / response.effectiveSpacingMeters);
		int lastZ = Math.Floor((typedRequest.centerZ + typedRequest.halfExtentMeters - boundsMin[2]) / response.effectiveSpacingMeters);
		response.gridWidth = lastX - firstX + 1;
		response.gridHeight = lastZ - firstZ + 1;
		if (response.gridWidth < 1 || response.gridHeight < 1 || response.gridWidth * response.gridHeight > 4096)
		{
			response.status = "invalid-sample-grid";
			return response;
		}
		response.gridOriginX = boundsMin[0] + firstX * response.effectiveSpacingMeters;
		response.gridOriginZ = boundsMin[2] + firstZ * response.effectiveSpacingMeters;
		for (int z = 0; z < response.gridHeight; z++)
		{
			for (int x = 0; x < response.gridWidth; x++)
			{
				if (!response.heights.IsEmpty())
					response.heights += ";";
				if (typedRequest.includeWater && !response.waterTypes.IsEmpty())
				{
					response.waterTypes += ";";
					response.waterSurfaceHeights += ";";
					response.waterDepthsAboveTerrain += ";";
				}
				float height;
				float sampleX = response.gridOriginX + x * response.effectiveSpacingMeters;
				float sampleZ = response.gridOriginZ + z * response.effectiveSpacingMeters;
				if (!api.TryGetTerrainSurfaceY(sampleX, sampleZ, height))
				{
					response.heights += "~";
					if (typedRequest.includeWater)
					{
						response.waterTypes += "~";
						response.waterSurfaceHeights += "~";
						response.waterDepthsAboveTerrain += "~";
					}
					continue;
				}
				response.heights += height.ToString();
				if (typedRequest.includeWater)
				{
					vector waterSurface;
					EWaterSurfaceType waterType;
					vector transformWS[4];
					vector obbExtents;
					float surfaceY;
					if (ChimeraWorldUtils.TryGetWaterSurface(world, Vector(sampleX, height, sampleZ), waterSurface, waterType, transformWS, obbExtents))
					{
						if (waterType == EWaterSurfaceType.WST_OCEAN)
							response.waterTypes += "o";
						else if (waterType == EWaterSurfaceType.WST_POND)
							response.waterTypes += "p";
						else if (waterType == EWaterSurfaceType.WST_RIVER)
							response.waterTypes += "r";
						else
							response.waterTypes += "n";
						surfaceY = waterSurface[1];
					}
					else
					{
						TraceParam waterTrace = new TraceParam();
						waterTrace.Start = Vector(sampleX, boundsMax[1] + 1, sampleZ);
						waterTrace.End = Vector(sampleX, boundsMin[1] - 1, sampleZ);
						waterTrace.Flags = TraceFlags.ENTS;
						waterTrace.TargetLayers = EPhysicsLayerDefs.Water;
						float traceFraction = world.TraceMove(waterTrace);
						if (traceFraction >= 1)
						{
							response.waterTypes += "n";
							response.waterSurfaceHeights += "~";
							response.waterDepthsAboveTerrain += "~";
							continue;
						}
						surfaceY = waterTrace.Start[1] + (waterTrace.End[1] - waterTrace.Start[1]) * traceFraction;
						if (surfaceY <= height)
						{
							response.waterTypes += "n";
							response.waterSurfaceHeights += "~";
							response.waterDepthsAboveTerrain += "~";
							continue;
						}
						if (waterTrace.TraceEnt && waterTrace.TraceEnt.Type().IsInherited(RiverPartEntity))
							response.waterTypes += "r";
						else
							response.waterTypes += "p";
					}
					response.waterSurfaceHeights += surfaceY.ToString();
					response.waterDepthsAboveTerrain += (surfaceY - height).ToString();
				}
			}
		}
		response.status = "available";
		return response;
	}
}
#endif
