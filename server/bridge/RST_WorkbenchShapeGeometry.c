#ifdef WORKBENCH
class RST_WorkbenchShapeGeometryRequest : JsonApiStruct
{
	string entityId;
	string operation;
	string fromSpace;
	string toSpace;
	string space;
	string points;
	string transformOperation;
	float offsetX;
	float offsetY;
	float offsetZ;
	float pivotX;
	float pivotY;
	float pivotZ;
	float degrees;
	float scaleX;
	float scaleY;
	float scaleZ;
	string mirrorAxis;
	float spacingMeters;

	void RST_WorkbenchShapeGeometryRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchShapeGeometryResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	string entity;
	string shapeClass;
	bool closed;
	string points;
	string fromSpace;
	string toSpace;
	float spacingMeters;
	int originalPointCount;
	int resultPointCount;
	float pathLength;
	int skippedZeroLengthSegments;

	void RST_WorkbenchShapeGeometryResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchShapeGeometry : NetApiHandler
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

	RST_WorkbenchShapeGeometryResponse Response()
	{
		RST_WorkbenchShapeGeometryResponse response = new RST_WorkbenchShapeGeometryResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		return response;
	}

	bool Setup(WorldEditorAPI api, RST_WorkbenchShapeGeometryResponse response)
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

	bool Resolve(WorldEditorAPI api, string entityId, RST_WorkbenchShapeGeometryResponse response, out IEntitySource source, out ShapeEntity shape)
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
		if (source.GetClassName() != "PolylineShapeEntity" && source.GetClassName() != "SplineShapeEntity")
		{
			response.status = "unsupported-shape-class";
			return false;
		}
		return true;
	}

	void Record(WorldEditorAPI api, IEntitySource source, ShapeEntity shape, RST_WorkbenchShapeGeometryResponse response)
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
		Encode(positions, response.points);
	}

	void Encode(array<vector> values, out string encoded)
	{
		encoded = string.Empty;
		foreach (vector point : values)
		{
			if (!encoded.IsEmpty())
				encoded += ";";
			encoded += string.Format("%1|%2|%3", point[0], point[1], point[2]);
		}
	}

	bool Decode(string encoded, out array<vector> decoded)
	{
		array<string> records = {
		};
		array<string> fields = {
		};
		if (encoded.IsEmpty())
			return true;
		encoded.Split(";", records, true);
		if (records.Count() > 4096)
			return false;
		foreach (string record : records)
		{
			fields.Clear();
			record.Split(",", fields, false);
			if (fields.Count() != 3)
				return false;
			decoded.Insert(Vector(fields[0].ToFloat(), fields[1].ToFloat(), fields[2].ToFloat()));
		}
		return true;
	}

	float Distance(vector a, vector b)
	{
		float x = b[0] - a[0];
		float y = b[1] - a[1];
		float z = b[2] - a[2];
		return Math.Sqrt(x * x + y * y + z * z);
	}

	vector Interpolate(vector a, vector b, float t)
	{
		return Vector(a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t);
	}

	void ToSpace(ShapeEntity shape, array<vector> values, string fromSpace, string toSpace)
	{
		if (fromSpace == toSpace)
			return;
		for (int i; i < values.Count(); i++)
		{
			if (fromSpace == "local")
				values[i] = shape.CoordToParent(values[i]);
			else
				values[i] = shape.CoordToLocal(values[i]);
		}
	}

	bool Commit(WorldEditorAPI api, IEntitySource source, ShapeEntity shape, array<vector> points, string label)
	{
		if (api.IsEntityLayerLockedHierarchy(source.GetSubScene(), source.GetLayerID()))
			return false;
		if (!api.BeginEntityAction(label))
			return false;
		// Materialize the native shape point state before replacing its authored
		// list. SetPoints otherwise does not reliably commit from a NET handler.
		shape.GetPointCount();
		shape.SetPoints(points, source);
		api.EndEntityAction(label);
		return true;
	}

	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchShapeGeometryRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchShapeGeometryRequest r = RST_WorkbenchShapeGeometryRequest.Cast(request);
		RST_WorkbenchShapeGeometryResponse response = Response();
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
		if (!Resolve(api, r.entityId, response, source, shape))
			return response;
		if (r.operation == "convert")
		{
			if ((r.fromSpace != "local" && r.fromSpace != "world") || (r.toSpace != "local" && r.toSpace != "world"))
			{
				response.status = "invalid-input";
				return response;
			}
			array<vector> responsePoints = {
			};
			if (!Decode(r.points, responsePoints))
			{
				response.status = "invalid-input";
				return response;
			}
			ToSpace(shape, responsePoints, r.fromSpace, r.toSpace);
			Encode(responsePoints, response.points);
			Record(api, source, shape, response);
			Encode(responsePoints, response.points);
			response.fromSpace = r.fromSpace;
			response.toSpace = r.toSpace;
			response.status = "converted";
			return response;
		}
		array<vector> points = {
		};
		shape.GetPointsPositions(points);
		if (r.operation == "transform")
		{
			if (r.space != "local" && r.space != "world")
			{
				response.status = "invalid-input";
				return response;
			}
			if (r.transformOperation == "reverse")
			{
				array<vector> reversed = {
				};
				for (int i = points.Count() - 1; i >= 0; i--)
				{
					reversed.Insert(points[i]);
				}
				points = reversed;
			}
			else
			{
				ToSpace(shape, points, "local", r.space);
				float radians = r.degrees * Math.PI / 180.0;
				float sine = Math.Sin(radians);
				float cosine = Math.Cos(radians);
				if (r.transformOperation == "scale" && (r.scaleX == 0 || r.scaleY == 0 || r.scaleZ == 0))
				{
					response.status = "invalid-input";
					return response;
				}
				if (r.transformOperation == "mirror" && r.mirrorAxis != "x" && r.mirrorAxis != "y" && r.mirrorAxis != "z")
				{
					response.status = "invalid-input";
					return response;
				}
				if (r.transformOperation != "translate" && r.transformOperation != "rotateXZ" && r.transformOperation != "scale" && r.transformOperation != "mirror")
				{
					response.status = "invalid-input";
					return response;
				}
				for (int i; i < points.Count(); i++)
				{
					vector p = points[i];
					if (r.transformOperation == "translate")
						p = p + Vector(r.offsetX, r.offsetY, r.offsetZ);
					else
					{
						p = p - Vector(r.pivotX, r.pivotY, r.pivotZ);
						if (r.transformOperation == "rotateXZ")
							p = Vector(p[0] * cosine - p[2] * sine, p[1], p[0] * sine + p[2] * cosine);
						else if (r.transformOperation == "scale")
							p = Vector(p[0] * r.scaleX, p[1] * r.scaleY, p[2] * r.scaleZ);
						else if (r.mirrorAxis == "x")
							p[0] = -p[0];
						else if (r.mirrorAxis == "y")
							p[1] = -p[1];
						else
							p[2] = -p[2];
						p = p + Vector(r.pivotX, r.pivotY, r.pivotZ);
					}
					points[i] = p;
				}
				ToSpace(shape, points, r.space, "local");
			}
			if (!Commit(api, source, shape, points, "Reforger Script Tools: transform shape points"))
			{
				response.status = "mutation-rejected";
				return response;
			}
			if (!Resolve(api, r.entityId, response, source, shape))
				return response;
			Record(api, source, shape, response);
			Encode(points, response.points);
			response.status = "points-transformed";
			return response;
		}
		if (r.operation != "resample")
		{
			response.status = "invalid-input";
			return response;
		}
		if (source.GetClassName() != "PolylineShapeEntity")
		{
			response.status = "entity-not-polyline";
			return response;
		}
		if (r.space != "local" && r.space != "world" || r.spacingMeters <= 0)
		{
			response.status = "invalid-input";
			return response;
		}
		int originalCount = points.Count();
		if (originalCount < 2)
		{
			response.status = "resample-rejected";
			return response;
		}
		ToSpace(shape, points, "local", r.space);
		array<vector> sampled = {
		};
		sampled.Insert(points[0]);
		float total = 0;
		int skipped = 0;
		int segments = points.Count() - 1;
		if (shape.IsClosed())
			segments++;
		for (int i; i < segments; i++)
		{
			vector a = points[i];
			vector b = points[(i + 1) % points.Count()];
			float length = Distance(a, b);
			if (length <= 0.00001)
			{
				skipped++;
				continue;
			}
			total += length;
		}
		if (total <= 0.00001)
		{
			response.status = "resample-rejected";
			return response;
		}
		float next = r.spacingMeters;
		float travelled = 0;
		for (int i; i < segments; i++)
		{
			vector a = points[i];
			vector b = points[(i + 1) % points.Count()];
			float length = Distance(a, b);
			if (length <= 0.00001)
				continue;
			while (next < travelled + length)
			{
				if (sampled.Count() >= 4096)
				{
					response.status = "resample-too-many-points";
					return response;
				}
				sampled.Insert(Interpolate(a, b, (next - travelled) / length));
				next += r.spacingMeters;
			}
			travelled += length;
		}
		if (!shape.IsClosed())
		{
			if (sampled.Count() >= 4096)
			{
				response.status = "resample-too-many-points";
				return response;
			}
			sampled.Insert(points[points.Count() - 1]);
		}
		ToSpace(shape, sampled, r.space, "local");
		if (!Commit(api, source, shape, sampled, "Reforger Script Tools: resample polyline"))
		{
			response.status = "mutation-rejected";
			return response;
		}
		if (!Resolve(api, r.entityId, response, source, shape))
			return response;
		Record(api, source, shape, response);
		Encode(sampled, response.points);
		response.spacingMeters = r.spacingMeters;
		response.originalPointCount = originalCount;
		response.resultPointCount = sampled.Count();
		response.pathLength = total;
		response.skippedZeroLengthSegments = skipped;
		response.status = "polyline-resampled";
		return response;
	}
}
#endif
