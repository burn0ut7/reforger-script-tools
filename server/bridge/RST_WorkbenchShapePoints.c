#ifdef WORKBENCH
class RST_WorkbenchShapePointsRequest : JsonApiStruct
{
	string entityId;
	string operation;
	int index;
	int count;
	string points;

	void RST_WorkbenchShapePointsRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchShapePointsResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	string entity;
	string shapeClass;
	bool closed;
	string points;

	void RST_WorkbenchShapePointsResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchShapePointsBase : NetApiHandler
{
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

	RST_WorkbenchShapePointsResponse Response()
	{
		RST_WorkbenchShapePointsResponse response = new RST_WorkbenchShapePointsResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		return response;
	}

	bool Setup(WorldEditorAPI api, RST_WorkbenchShapePointsResponse response)
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

	bool ResolveShape(WorldEditorAPI api, string entityId, RST_WorkbenchShapePointsResponse response, out IEntitySource source, out ShapeEntity shape)
	{
		source = Find(api, entityId);
		if (!source)
		{
			response.status = "entity-not-found";
			return false;
		}
		shape = ShapeEntity.Cast(api.SourceToEntity(source));
		if (!shape)
		{
			response.status = "entity-not-shape";
			return false;
		}
		return true;
	}

	void Record(WorldEditorAPI api, IEntitySource source, ShapeEntity shape, RST_WorkbenchShapePointsResponse response)
	{
		vector origin = shape.GetOrigin();
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
		response.closed = shape.IsClosed();
		array<vector> positions = {
		};
		shape.GetPointsPositions(positions);
		foreach (vector point : positions)
		{
			if (!response.points.IsEmpty())
				response.points += ";";
			response.points += string.Format("%1|%2|%3", point[0], point[1], point[2]);
		}
	}

	bool DecodePoints(string encoded, out array<vector> decoded)
	{
		if (encoded.IsEmpty())
			return true;
		array<string> records = {
		};
		encoded.Split(";", records, true);
		foreach (string record : records)
		{
			array<string> fields = {
			};
			record.Split(",", fields, false);
			if (fields.Count() != 3)
				return false;
			decoded.Insert(Vector(fields[0].ToFloat(), fields[1].ToFloat(), fields[2].ToFloat()));
		}
		return true;
	}
}

class RST_WorkbenchShapePoints : RST_WorkbenchShapePointsBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchShapePointsRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchShapePointsRequest r = RST_WorkbenchShapePointsRequest.Cast(request);
		RST_WorkbenchShapePointsResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!Setup(api, response))
			return response;
		IEntitySource source;
		ShapeEntity shape;
		if (!ResolveShape(api, r.entityId, response, source, shape))
			return response;
		Record(api, source, shape, response);
		response.status = "available";
		return response;
	}
}

class RST_WorkbenchEditShapePoints : RST_WorkbenchShapePointsBase
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchShapePointsRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchShapePointsRequest r = RST_WorkbenchShapePointsRequest.Cast(request);
		RST_WorkbenchShapePointsResponse response = Response();
		WorldEditor editor = Workbench.GetModule(WorldEditor);
		if (!editor)
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		WorldEditorAPI api = editor.GetApi();
		if (!Setup(api, response))
			return response;
		IEntitySource source;
		ShapeEntity shape;
		if (!ResolveShape(api, r.entityId, response, source, shape))
			return response;
		if (api.IsEntityLayerLockedHierarchy(source.GetSubScene(), source.GetLayerID()))
		{
			response.status = "mutation-rejected";
			return response;
		}
		array<vector> current = {
		};
		shape.GetPointsPositions(current);
		array<vector> supplied = {
		};
		if (!DecodePoints(r.points, supplied))
		{
			response.status = "invalid-points";
			return response;
		}
		if (r.operation == "set")
			current = supplied;
		else if (r.operation == "insert")
		{
			if (supplied.IsEmpty() || r.index < 0 || r.index > current.Count())
			{
				response.status = "invalid-point-edit";
				return response;
			}
			foreach (vector point : supplied)
			{
				if (r.index == current.Count())
					current.Insert(point);
				else
					current.InsertAt(point, r.index);
				r.index++;
			}
		}
		else if (r.operation == "delete")
		{
			if (r.count < 1 || r.index < 0 || r.index >= current.Count() || r.count > current.Count() - r.index)
			{
				response.status = "invalid-point-edit";
				return response;
			}
			for (int i; i < r.count; i++)
			{
				current.RemoveOrdered(r.index);
			}
		}
		else
		{
			response.status = "invalid-point-edit";
			return response;
		}
		if (!api.BeginEntityAction("Reforger Script Tools: edit shape points"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		shape.SetPoints(current, source);
		api.EndEntityAction("Reforger Script Tools: edit shape points");
		if (!ResolveShape(api, r.entityId, response, source, shape))
			return response;
		Record(api, source, shape, response);
		response.status = "points-updated";
		return response;
	}
}
#endif
