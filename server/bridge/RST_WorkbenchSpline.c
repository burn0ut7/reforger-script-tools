#ifdef WORKBENCH
class RST_WorkbenchSplineRequest : JsonApiStruct
{
	string entityId;
	string operation;
	string space;
	string anchors;
	bool hasClosed;
	bool closed;
	int maxSamples;

	void RST_WorkbenchSplineRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchSplineAnchor
{
	int index;
	string tangentMode;
	vector position;
	vector inTangent;
	vector outTangent;
}

class RST_WorkbenchSplineResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	string entity;
	string shapeClass;
	bool closed;
	int anchorCount;
	string anchors;
	string samples;
	string sampleSpace;
	int sampleCount;
	float pathLength;

	void RST_WorkbenchSplineResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchSpline : NetApiHandler
{
	protected static const int MAX_POINTS = 4096;

	IEntitySource Find(WorldEditorAPI api, string entityId)
	{
		for (int i, count = api.GetEditorEntityCount(); i < count; i++)
		{
			IEntitySource candidate = api.GetEditorEntity(i);
			if (candidate && candidate.GetID().ToString() == entityId)
				return candidate;
		}
		return null;
	}

	RST_WorkbenchSplineResponse Response()
	{
		RST_WorkbenchSplineResponse response = new RST_WorkbenchSplineResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		response.sampleSpace = "local";
		return response;
	}

	bool Setup(WorldEditorAPI api, RST_WorkbenchSplineResponse response)
	{
		if (!api)
		{
			response.status = "world-editor-api-unavailable";
			return false;
		}
		if (!api.GetWorld())
		{
			response.status = "world-unavailable";
			return false;
		}
		if (api.IsPrefabEditMode())
		{
			response.status = "prefab-edit-mode";
			return false;
		}
		if (api.IsDoingEditAction())
		{
			response.status = "editor-action-active";
			return false;
		}
		return true;
	}

	bool Resolve(WorldEditorAPI api, string entityId, RST_WorkbenchSplineResponse response, out IEntitySource source, out SplineShapeEntity spline)
	{
		source = Find(api, entityId);
		if (!source)
		{
			response.status = "entity-not-found";
			return false;
		}
		spline = SplineShapeEntity.Cast(api.SourceToEntity(source));
		if (!spline)
		{
			response.status = "entity-not-spline";
			return false;
		}
		return true;
	}

	vector PointToSpace(SplineShapeEntity spline, vector point, string space)
	{
		if (space == "world")
			return spline.CoordToParent(point);
		return point;
	}

	vector TangentToSpace(SplineShapeEntity spline, vector localAnchor, vector localTangent, string space)
	{
		if (space == "world")
			return spline.CoordToParent(localAnchor + localTangent) - spline.CoordToParent(localAnchor);
		return localTangent;
	}

	vector PointToLocal(SplineShapeEntity spline, vector point, string space)
	{
		if (space == "world")
			return spline.CoordToLocal(point);
		return point;
	}

	vector TangentToLocal(SplineShapeEntity spline, vector spaceAnchor, vector spaceTangent, string space)
	{
		if (space == "world")
			return spline.CoordToLocal(spaceAnchor + spaceTangent) - spline.CoordToLocal(spaceAnchor);
		return spaceTangent;
	}

	void EncodeVectors(array<vector> values, out string encoded)
	{
		encoded = string.Empty;
		foreach (vector value : values)
		{
			if (!encoded.IsEmpty())
				encoded += ";";
			encoded += string.Format("%1,%2,%3", value[0], value[1], value[2]);
		}
	}

	void EncodeAnchors(SplineShapeEntity spline, string space, out string encoded, out int anchorCount)
	{
		encoded = string.Empty;
		anchorCount = 0;
		array<vector> localPositions = {};
		spline.GetPointsPositions(localPositions);
		anchorCount = localPositions.Count();
		int encodedCount = Math.Min(anchorCount, MAX_POINTS);
		for (int i; i < encodedCount; i++)
		{
			vector localPosition = localPositions[i];
			vector inTangent;
			vector outTangent;
			spline.GetTangents(i, inTangent, outTangent);
			string tangentMode = "auto";
			if (spline.HasPointExplicitTangents(i))
				tangentMode = "explicit";
			if (!encoded.IsEmpty())
				encoded += ";";
			encoded += string.Format(
				"%1,%2,%3,%4,%5,%6,%7,%8,%9",
				i,
				tangentMode,
				PointToSpace(spline, localPosition, space)[0],
				PointToSpace(spline, localPosition, space)[1],
				PointToSpace(spline, localPosition, space)[2],
				TangentToSpace(spline, localPosition, inTangent, space)[0],
				TangentToSpace(spline, localPosition, inTangent, space)[1],
				TangentToSpace(spline, localPosition, inTangent, space)[2],
				TangentToSpace(spline, localPosition, outTangent, space)[0]);
			encoded += string.Format(
				",%1,%2",
				TangentToSpace(spline, localPosition, outTangent, space)[1],
				TangentToSpace(spline, localPosition, outTangent, space)[2]);
		}
	}

	void Record(WorldEditorAPI api, IEntitySource source, SplineShapeEntity spline, string space, RST_WorkbenchSplineResponse response)
	{
		vector origin = spline.GetOrigin();
		string resourceName = string.Format("%1", source.GetResourceName());
		string name = source.GetName();
		string subSceneName = api.GetWorld().GetSubSceneName(source.GetSubScene());
		string layerName = api.GetEntitySubsceneLayer(source.GetSubScene(), source);
		if (name == resourceName)
			name = string.Empty;
		resourceName.Replace("|", "/");
		resourceName.Replace(";", "/");
		name.Replace("|", "/");
		name.Replace(";", "/");
		subSceneName.Replace("|", "/");
		subSceneName.Replace(";", "/");
		layerName.Replace("|", "/");
		layerName.Replace(";", "/");
		response.entity = string.Format("%1|%2|%3|%4|%5|%6|%7", source.GetID().ToString(), source.GetClassName(), source.GetSubScene(), source.GetLayerID(), origin[0], origin[1], origin[2]) + "|" + resourceName + "|" + name + "|" + subSceneName + "|" + layerName;
		response.shapeClass = source.GetClassName();
		response.closed = spline.IsClosed();
		response.sampleSpace = space;
		EncodeAnchors(spline, space, response.anchors, response.anchorCount);
		array<vector> nativeSamples = {};
		spline.GenerateTesselatedShape(nativeSamples);
		response.pathLength = PathLengthInSpace(spline, nativeSamples, space);
	}

	bool DecodeAnchors(string encoded, out array<ref RST_WorkbenchSplineAnchor> anchors)
	{
		anchors = {};
		array<string> records = {};
		array<string> fields = {};
		if (encoded.IsEmpty())
			return false;
		encoded.Split(";", records, true);
		if (records.Count() < 2 || records.Count() > MAX_POINTS)
			return false;
		foreach (string record : records)
		{
			fields.Clear();
			record.Split(",", fields, false);
			if (fields.Count() != 11 || (fields[1] != "auto" && fields[1] != "explicit"))
				return false;
			RST_WorkbenchSplineAnchor anchor = new RST_WorkbenchSplineAnchor();
			anchor.index = fields[0].ToInt();
			anchor.tangentMode = fields[1];
			anchor.position = Vector(fields[2].ToFloat(), fields[3].ToFloat(), fields[4].ToFloat());
			anchor.inTangent = Vector(fields[5].ToFloat(), fields[6].ToFloat(), fields[7].ToFloat());
			anchor.outTangent = Vector(fields[8].ToFloat(), fields[9].ToFloat(), fields[10].ToFloat());
			if (anchor.index != anchors.Count())
				return false;
			anchors.Insert(anchor);
		}
		return true;
	}

	bool ClearPointData(WorldEditorAPI api, IEntitySource source, int index)
	{
		array<ref ContainerIdPathEntry> pointPath = { new ContainerIdPathEntry("Points", index) };
		auto point = source.GetObjectArray("Points").Get(index);
		auto data = point.GetObjectArray("Data");
		for (int i = data.Count() - 1; i >= 0; i--)
		{
			if (!api.RemoveObjectArrayVariableMember(source, pointPath, "Data", i))
				return false;
		}
		return true;
	}

	bool WriteExplicitTangents(WorldEditorAPI api, IEntitySource source, int index, vector inTangent, vector outTangent)
	{
		array<ref ContainerIdPathEntry> pointPath = { new ContainerIdPathEntry("Points", index) };
		if (!ClearPointData(api, source, index))
			return false;
		if (!api.CreateObjectArrayVariableMember(source, pointPath, "Data", "SplinePointData", 0))
			return false;
		array<ref ContainerIdPathEntry> dataPath = { new ContainerIdPathEntry("Points", index), new ContainerIdPathEntry("Data", 0) };
		return api.SetVariableValue(source, dataPath, "InTangent", inTangent.ToString(false))
			&& api.SetVariableValue(source, dataPath, "OutTangent", outTangent.ToString(false));
	}

	bool WriteSpline(WorldEditorAPI api, IEntitySource source, SplineShapeEntity spline, string space, array<ref RST_WorkbenchSplineAnchor> anchors, bool hasClosed, bool closed)
	{
		if (api.IsEntityLayerLockedHierarchy(source.GetSubScene(), source.GetLayerID()))
			return false;
		if (!api.BeginEntityAction("Reforger Script Tools: edit spline"))
			return false;

		auto points = source.GetObjectArray("Points");
		while (points.Count() > anchors.Count())
		{
			if (!api.RemoveObjectArrayVariableMember(source, null, "Points", points.Count() - 1))
			{
				api.EndEntityAction("Reforger Script Tools: edit spline");
				return false;
			}
			points = source.GetObjectArray("Points");
		}
		while (points.Count() < anchors.Count())
		{
			if (!api.CreateObjectArrayVariableMember(source, null, "Points", "ShapePoint", points.Count()))
			{
				api.EndEntityAction("Reforger Script Tools: edit spline");
				return false;
			}
			points = source.GetObjectArray("Points");
		}

		foreach (int i, RST_WorkbenchSplineAnchor anchor : anchors)
		{
			vector localPosition = PointToLocal(spline, anchor.position, space);
			vector localInTangent = TangentToLocal(spline, anchor.position, anchor.inTangent, space);
			vector localOutTangent = TangentToLocal(spline, anchor.position, anchor.outTangent, space);
			array<ref ContainerIdPathEntry> pointPath = { new ContainerIdPathEntry("Points", i) };
			if (!api.SetVariableValue(source, pointPath, "Position", localPosition.ToString(false)))
			{
				api.EndEntityAction("Reforger Script Tools: edit spline");
				return false;
			}
			if (anchor.tangentMode == "explicit")
			{
				if (!WriteExplicitTangents(api, source, i, localInTangent, localOutTangent))
				{
					api.EndEntityAction("Reforger Script Tools: edit spline");
					return false;
				}
			}
			else if (!ClearPointData(api, source, i))
			{
				api.EndEntityAction("Reforger Script Tools: edit spline");
				return false;
			}
		}
		string closedValue = "0";
		if (closed)
			closedValue = "1";
		if (hasClosed && !api.SetVariableValue(source, null, "IsClosed", closedValue))
		{
			api.EndEntityAction("Reforger Script Tools: edit spline");
			return false;
		}
		api.EndEntityAction("Reforger Script Tools: edit spline");
		return true;
	}

	float Distance(vector a, vector b)
	{
		vector delta = b - a;
		return Math.Sqrt(delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]);
	}

	float PathLengthInSpace(SplineShapeEntity spline, array<vector> localPoints, string space)
	{
		float length;
		vector previous;
		bool hasPrevious;
		foreach (vector localPoint : localPoints)
		{
			vector point = PointToSpace(spline, localPoint, space);
			if (hasPrevious)
				length += Distance(previous, point);
			previous = point;
			hasPrevious = true;
		}
		return length;
	}

	void Sample(SplineShapeEntity spline, string space, int maxSamples, out string encoded, out float pathLength, out int sampleCount)
	{
		encoded = string.Empty;
		pathLength = 0;
		sampleCount = 0;
		array<vector> nativeSamples = {};
		spline.GenerateTesselatedShape(nativeSamples);
		pathLength = PathLengthInSpace(spline, nativeSamples, space);
		if (nativeSamples.Count() < 2)
			return;
		int target = Math.Min(maxSamples, nativeSamples.Count());
		for (int i; i < target; i++)
		{
			int sourceIndex = i * (nativeSamples.Count() - 1) / (target - 1);
			vector output = PointToSpace(spline, nativeSamples[sourceIndex], space);
			if (!encoded.IsEmpty())
				encoded += ";";
			encoded += string.Format("%1,%2,%3", output[0], output[1], output[2]);
		}
		sampleCount = target;
	}

	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchSplineRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchSplineRequest r = RST_WorkbenchSplineRequest.Cast(request);
		RST_WorkbenchSplineResponse response = Response();
		WorldEditor worldEditor = Workbench.GetModule(WorldEditor);
		if (!worldEditor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = worldEditor.GetApi();
		if (!Setup(api, response))
			return response;
		IEntitySource source;
		SplineShapeEntity spline;
		if (!Resolve(api, r.entityId, response, source, spline))
			return response;
		if (r.space != "local" && r.space != "world")
		{
			response.status = "invalid-input";
			return response;
		}
		if (r.operation == "inspect")
		{
			Record(api, source, spline, r.space, response);
			response.status = "available";
			return response;
		}
		if (r.operation == "sample")
		{
			if (r.maxSamples < 2 || r.maxSamples > MAX_POINTS)
			{
				response.status = "invalid-input";
				return response;
			}
			Record(api, source, spline, r.space, response);
			Sample(spline, r.space, r.maxSamples, response.samples, response.pathLength, response.sampleCount);
			response.status = "sampled";
			return response;
		}
		if (r.operation != "edit")
		{
			response.status = "invalid-input";
			return response;
		}
		array<ref RST_WorkbenchSplineAnchor> anchors;
		if (!DecodeAnchors(r.anchors, anchors) || !WriteSpline(api, source, spline, r.space, anchors, r.hasClosed, r.closed))
		{
			response.status = "mutation-rejected";
			return response;
		}
		if (!Resolve(api, r.entityId, response, source, spline))
			return response;
		Record(api, source, spline, r.space, response);
		response.status = "spline-updated";
		return response;
	}
}
#endif
