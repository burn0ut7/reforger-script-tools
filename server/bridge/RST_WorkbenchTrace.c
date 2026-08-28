#ifdef WORKBENCH
class RST_WorkbenchTraceRequest : JsonApiStruct
{
	float startX;
	float startY;
	float startZ;
	float endX;
	float endY;
	float endZ;
	string shape;
	float radius;
	float minsX;
	float minsY;
	float minsZ;
	float maxsX;
	float maxsY;
	float maxsZ;
	bool entities;
	bool terrain;
	bool ocean;
	int targetLayers;

	void RST_WorkbenchTraceRequest()
	{
		RegAll();
	}
}

class RST_WorkbenchTraceResponse : JsonApiStruct
{
	string bridgeVersion;
	int protocolVersion;
	string status;
	bool hit;
	float fraction;
	float distance;
	float hitX;
	float hitY;
	float hitZ;
	float normalX;
	float normalY;
	float normalZ;
	string kind;
	string entity;
	string colliderName;
	string material;

	void RST_WorkbenchTraceResponse()
	{
		RegAll();
	}
}

class RST_WorkbenchTrace : NetApiHandler
{
	override JsonApiStruct GetRequest()
	{
		return new RST_WorkbenchTraceRequest();
	}

	override JsonApiStruct GetResponse(JsonApiStruct request)
	{
		RST_WorkbenchTraceRequest q = RST_WorkbenchTraceRequest.Cast(request);
		RST_WorkbenchTraceResponse response = new RST_WorkbenchTraceResponse();
		response.bridgeVersion = "1.52.13";
		response.protocolVersion = 1;
		WorldEditor e = Workbench.GetModule(WorldEditor);
		if (!e || !e.GetApi() || !e.GetApi().GetWorld())
		{
			response.status = "world-editor-unavailable";
			return response;
		}
		TraceParam p;
		if (q.shape == "sphere")
		{
			TraceSphere s = new TraceSphere();
			s.Radius = q.radius;
			p = s;
		}
		else if (q.shape == "box")
		{
			TraceBox b = new TraceBox();
			b.Mins = Vector(q.minsX, q.minsY, q.minsZ);
			b.Maxs = Vector(q.maxsX, q.maxsY, q.maxsZ);
			p = b;
		}
		else if (q.shape == "line")
			p = new TraceParam();
		else
		{
			response.status = "invalid-query";
			return response;
		}
		p.Start = Vector(q.startX, q.startY, q.startZ);
		p.End = Vector(q.endX, q.endY, q.endZ);
		if (q.entities)
			p.Flags |= TraceFlags.ENTS;
		if (q.terrain)
			p.Flags |= TraceFlags.WORLD;
		if (q.ocean)
			p.Flags |= TraceFlags.OCEAN;
		if (q.targetLayers != 0)
			p.TargetLayers = q.targetLayers;
		response.fraction = e.GetApi().GetWorld().TraceMove(p);
		if (response.fraction >= 1)
		{
			response.status = "available";
			return response;
		}
		vector h = p.Start + (p.End - p.Start) * response.fraction;
		vector travelled = h - p.Start;
		response.hit = true;
		response.hitX = h[0];
		response.hitY = h[1];
		response.hitZ = h[2];
		response.distance = Math.Sqrt(travelled[0] * travelled[0] + travelled[1] * travelled[1] + travelled[2] * travelled[2]);
		response.normalX = p.TraceNorm[0];
		response.normalY = p.TraceNorm[1];
		response.normalZ = p.TraceNorm[2];
		response.colliderName = p.ColliderName;
		response.material = p.TraceMaterial;
		if (p.TraceEnt && !p.TraceEnt.Type().IsInherited(GenericTerrainEntity))
		{
			response.kind = "entity";
			IEntitySource source = e.GetApi().EntityToSource(p.TraceEnt);
			if (source)
				response.entity = string.Format("%1|%2|%3|%4", source.GetID().ToString(), source.GetClassName(), source.GetSubScene(), source.GetLayerID());
		}
		else if (p.TraceEnt)
			response.kind = "terrain";
		else if (q.ocean)
			response.kind = "ocean";
		else
			response.kind = "terrain";
		response.status = "available";
		return response;
	}
}
#endif
